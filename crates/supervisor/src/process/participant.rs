//! Supervised child-process lifecycle and restart handling.

use super::{
    ParticipantSpec, ProcessState, RestartPolicy, SupervisorState, ensure_process_group_stopped,
    join_reader, kill_child_process_group, requested_stop_exit_is_clean, send_process_group_signal,
    send_process_signal, spawn_output_reader, stop_child,
};
use crate::ManagedChild;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::identity::ProducerId;
use phoxal_cli_core::runtime::{ExitDescription, ProcessFailureKind, ReadinessPolicy};
use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::timeout;

pub(crate) struct RunningParticipant {
    pub(crate) spec: ParticipantSpec,
    pub(crate) shutdown_phase: String,
    pub(crate) child: Option<ManagedChild>,
    pub(crate) stdout_task: Option<JoinHandle<()>>,
    pub(crate) stderr_task: Option<JoinHandle<()>>,
    pub(crate) failure_times: VecDeque<Instant>,
    pub(crate) restart_count: u32,
    pub(crate) restart_at: Option<Instant>,
    pub(crate) failed: bool,
}

impl Drop for RunningParticipant {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        if let Some(pid) = child.id() {
            #[cfg(unix)]
            if self.spec.process_group {
                if let Err(error) = send_process_group_signal(pid, libc::SIGKILL) {
                    tracing::warn!(
                        participant = %self.spec.id,
                        pid,
                        error = %error,
                        "failed to kill supervised process group while dropping participant"
                    );
                }
            } else if let Err(error) = send_process_signal(pid, libc::SIGKILL) {
                tracing::warn!(
                    participant = %self.spec.id,
                    pid,
                    error = %error,
                    "failed to kill supervised child while dropping participant"
                );
            }
            #[cfg(not(unix))]
            if let Err(error) = {
                let mut child = child;
                child.start_kill()
            } {
                tracing::warn!(
                    participant = %self.spec.id,
                    pid,
                    error = %error,
                    "failed to kill supervised child while dropping participant"
                );
            }
        }
    }
}

impl RunningParticipant {
    pub(crate) async fn spawn_in_phase(
        spec: ParticipantSpec,
        board: &SupervisorState,
        phase: impl Into<String>,
    ) -> Result<Self> {
        let mut running = Self {
            spec,
            shutdown_phase: phase.into(),
            child: None,
            stdout_task: None,
            stderr_task: None,
            failure_times: VecDeque::new(),
            restart_count: 0,
            restart_at: None,
            failed: false,
        };
        running.spawn_child(board).await?;
        Ok(running)
    }

    pub(crate) async fn spawn_child(&mut self, board: &SupervisorState) -> Result<()> {
        // Failure evidence belongs to one concrete child incarnation. A later
        // restart must never attribute the previous process's stderr to a new
        // failure.
        board.clear_captured_stderr(&self.spec.key);
        // Every (re)spawn returns to `Starting`. A first appearance promotes
        // the stable participant identity to `Ready`; an identity that is
        // continuously present across replacement remains `Ready` below.
        board.set_state(
            &self.spec.key,
            ProcessState::Starting,
            self.spec.note.clone(),
        );
        if matches!(self.spec.readiness, ReadinessPolicy::ExactLiveliness(_)) {
            // Nothing mints a producer any more: a participant's producer is
            // the ZID of the Zenoh session it opens, which does not exist yet.
            // Clearing it here is what makes the snapshot report a fact - this
            // incarnation has no session - until its liveliness token arrives.
            board.clear_producer(&self.spec.key);
        }
        let mut command = Command::new(&self.spec.executable);
        command.args(&self.spec.args);
        if let Some(cwd) = &self.spec.cwd {
            command.current_dir(cwd);
        }
        command
            // No terminal input for any child: the TUI (Part 4) owns terminal
            // input exclusively (there is no Interact/raw-io tab), and a
            // child that inherited this process's stdin could otherwise race
            // it for keystrokes.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ManagedChild::spawn(&mut command, self.spec.process_group, &self.spec.env)
            .with_context(|| format!("failed to spawn {}", self.spec.command_line()))?;
        let pid = child.id();
        board.set_pid(&self.spec.key, pid);
        let pid = pid.unwrap_or_default();
        tracing::debug!(process = %self.spec.key, pid, "spawned supervised process");
        if let Some(stdout) = child.stdout.take() {
            self.stdout_task = Some(spawn_output_reader(
                board.clone(),
                self.spec.key.clone(),
                "stdout",
                stdout,
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            self.stderr_task = Some(spawn_output_reader(
                board.clone(),
                self.spec.key.clone(),
                "stderr",
                stderr,
            ));
        }
        self.child = Some(child);
        if let ReadinessPolicy::ExactLiveliness(instance) = &self.spec.readiness {
            // OBSERVED readiness (not spawn-is-ready): a bus participant stays
            // `Starting` (set at the top of this function) until the
            // supervisor's history-enabled observer sees its Liveliness key -
            // see `SupervisorState::record_instance_presence`. A
            // process that spawned successfully but never gets that far
            // (crashed before `#[setup]` completed, hung, or was silently
            // never launched by Webots) must never be reported ready. If a
            // replacement overlaps a stale holder of the same stable key,
            // Zenoh reports continuous presence rather than another `Alive`;
            // preserve that known binary state instead of inventing an
            // producer signal or waiting for a duplicate event.
            if board.is_exact_present(instance) {
                board.set_state(&self.spec.key, ProcessState::Ready, self.spec.note.clone());
            }
        } else {
            // No bus identity to observe (e.g. the Webots application itself,
            // see `ParticipantSpec::bus_participant`) - readiness is
            // necessarily process-lifecycle only.
            board.set_state(&self.spec.key, ProcessState::Ready, self.spec.note.clone());
        }
        Ok(())
    }

    fn record_spawn_failure(
        &mut self,
        board: &SupervisorState,
        operation: &str,
        error: &anyhow::Error,
    ) {
        self.child = None;
        self.restart_at = None;
        self.failed = true;
        board.set_pid(&self.spec.key, None);
        let detail = format!("{operation} failed: {error:#}");
        tracing::warn!(process = %self.spec.key, %detail, "supervised process spawn failed");
        board.record_failure(&self.spec.key, ProcessFailureKind::Spawn, None, detail);
    }

    pub(crate) async fn wait_for_requested_stop(
        &mut self,
        board: &SupervisorState,
        budget: Duration,
        terminate_sent: bool,
    ) -> Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        let pid = child.id();
        let status = match timeout(budget, child.wait()).await {
            Ok(status) => status.context("failed to wait for requested child stop")?,
            Err(_) => return Ok(false),
        };
        // The leader has been reaped: clear its PID before any fallible
        // descendant cleanup so forced-shutdown code can never signal a
        // recycled process-group id.
        self.child = None;
        board.set_pid(&self.spec.key, None);
        let group_stop = async {
            if self.spec.process_group
                && let Some(pid) = pid
            {
                ensure_process_group_stopped(pid).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;
        group_stop?;
        self.failed = true;
        if requested_stop_exit_is_clean(&status, terminate_sent) {
            board.set_state(
                &self.spec.key,
                ProcessState::Stopped,
                Some(format!("stopped after requested SIGTERM ({status})")),
            );
        } else {
            board.set_state(
                &self.spec.key,
                ProcessState::Failed,
                Some(format!(
                    "exited independently during requested stop ({status})"
                )),
            );
        }
        Ok(true)
    }

    pub(crate) async fn kill_process_group_after_timeout(
        &mut self,
        board: &SupervisorState,
    ) -> Result<()> {
        if !self.spec.process_group {
            bail!(
                "requested-stop fallback requires an isolated process group for {}",
                self.spec.id
            );
        }
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        tracing::warn!(process = %self.spec.key, "SIGTERM grace expired; killing process group");
        if let Err(error) = kill_child_process_group(&mut child).await {
            self.child = Some(child);
            return Err(error);
        }
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;
        self.failed = true;
        board.set_state(
            &self.spec.key,
            ProcessState::Failed,
            Some("SIGTERM grace expired; SIGKILL fallback used".to_string()),
        );
        Ok(())
    }

    pub(crate) async fn poll(
        &mut self,
        board: &SupervisorState,
        policy: &RestartPolicy,
    ) -> Result<()> {
        if self.failed {
            return Ok(());
        }
        if let Some(restart_at) = self.restart_at {
            if Instant::now() < restart_at {
                return Ok(());
            }
            self.restart_at = None;
            if let Err(error) = self.spawn_child(board).await {
                self.record_spawn_failure(board, "restart spawn", &error);
            }
            return Ok(());
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let pid = child.id();
        let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to poll {}", self.spec.id))?
        else {
            return Ok(());
        };

        self.child = None;
        board.set_pid(&self.spec.key, None);
        let group_stop = async {
            if self.spec.process_group
                && let Some(pid) = pid
            {
                ensure_process_group_stopped(pid).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;
        group_stop?;

        if status.success() {
            self.failed = true;
            board.record_failure(
                &self.spec.key,
                ProcessFailureKind::Exit,
                Some(exit_description(&status)),
                "exited successfully; supervisor expected a long-running participant",
            );
            return Ok(());
        }

        let now = Instant::now();
        self.failure_times
            .retain(|failure| now.duration_since(*failure) <= policy.start_limit_interval);
        self.failure_times.push_back(now);
        if self.failure_times.len() >= policy.start_limit_burst {
            self.failed = true;
            board.record_failure(
                &self.spec.key,
                ProcessFailureKind::Exit,
                Some(exit_description(&status)),
                format!(
                    "StartLimitBurst exhausted after {} failures in {}; last status {status}",
                    policy.start_limit_burst,
                    crate::format_duration(policy.start_limit_interval)
                ),
            );
            return Ok(());
        }

        self.restart_count = self.restart_count.saturating_add(1);
        board.set_restart_count(&self.spec.key, self.restart_count);
        board.set_state(
            &self.spec.key,
            ProcessState::Restarting,
            Some(format!(
                "exited with {status}; restarting in {}",
                crate::format_duration(policy.restart_delay)
            )),
        );
        self.restart_at = Some(now + policy.restart_delay);
        Ok(())
    }

    pub(crate) async fn stop_current(&mut self, board: &SupervisorState) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            tracing::debug!(process = %self.spec.key, "stopping supervised process");
            stop_child(
                &mut child,
                self.spec.shutdown_grace,
                self.spec.process_group,
            )
            .await?;
            board.set_pid(&self.spec.key, None);
        }
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;
        Ok(())
    }

    pub(crate) async fn swap(
        &mut self,
        spec: ParticipantSpec,
        board: &SupervisorState,
        note: String,
    ) -> Result<()> {
        self.stop_current(board).await?;
        self.spec = spec;
        self.failed = false;
        self.restart_at = None;
        if let Err(error) = self.spawn_child(board).await {
            self.record_spawn_failure(board, "swap spawn", &error);
            return Ok(());
        }
        tracing::info!(process = %self.spec.key, %note, "replaced supervised process");
        Ok(())
    }
}

fn exit_description(status: &std::process::ExitStatus) -> ExitDescription {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitDescription {
            code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        ExitDescription {
            code: status.code(),
            signal: None,
        }
    }
}
