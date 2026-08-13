//! Supervised child-process lifecycle and restart handling.

use super::child::ManagedChild;
use super::output::{join_reader, spawn_output_reader};
use super::signals::{ensure_process_group_stopped, send_process_group_signal, stop_child};
use super::stages::format_duration;
use crate::model::launch::{ParticipantSpec, RestartPolicy};
use crate::model::process::{ExitDescription, ProcessFailureKind, ProcessState};
use crate::state::store::SupervisorState;
use anyhow::Context;
use anyhow::Result;
use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Instant;
use tokio::process::Command;
use tokio::task::JoinHandle;

pub(crate) struct RunningParticipant {
    pub(crate) spec: ParticipantSpec,
    pub(crate) shutdown_stage: super::stages::SupervisionStageKind,
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
        if let Some(pid) = child.id()
            && let Err(error) = send_process_group_signal(pid, libc::SIGKILL)
        {
            tracing::warn!(
                participant = %self.spec.key,
                pid,
                error = %error,
                "failed to kill supervised process group while dropping participant"
            );
        }
    }
}

impl RunningParticipant {
    pub(crate) async fn spawn_in_stage(
        spec: ParticipantSpec,
        board: &SupervisorState,
        stage: super::stages::SupervisionStageKind,
    ) -> Result<Self> {
        let mut running = Self {
            spec,
            shutdown_stage: stage,
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
        board.set_state(&self.spec.key, ProcessState::Starting);
        // A participant's producer is the ZID of the bus session it has not yet
        // opened. Fence the previous incarnation before spawning its replacement.
        board.prepare_incarnation(&self.spec.key);
        let mut command = Command::new(&self.spec.executable);
        command.args(&self.spec.args);
        command
            // No terminal input for any child: the TUI owns terminal
            // input exclusively (there is no Interact/raw-io tab), and a
            // child that inherited this process's stdin could otherwise race
            // it for keystrokes.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ManagedChild::spawn(&mut command)
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
            .with_context(|| format!("failed to poll {}", self.spec.key))?
        else {
            return Ok(());
        };

        self.child = None;
        board.set_pid(&self.spec.key, None);
        let group_stop = async {
            if let Some(pid) = pid {
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
                    format_duration(policy.start_limit_interval)
                ),
            );
            return Ok(());
        }

        self.restart_count = self.restart_count.saturating_add(1);
        board.record_restart(&self.spec.key);
        board.set_state(&self.spec.key, ProcessState::Restarting);
        self.restart_at = Some(now + policy.restart_delay);
        Ok(())
    }

    pub(crate) async fn stop_current(&mut self, board: &SupervisorState) -> Result<()> {
        let stop = if let Some(mut child) = self.child.take() {
            tracing::debug!(process = %self.spec.key, "stopping supervised process");
            let result = stop_child(&mut child, self.spec.shutdown_grace).await;
            board.set_pid(&self.spec.key, None);
            result
        } else {
            Ok(())
        };
        join_reader(self.stdout_task.take()).await;
        join_reader(self.stderr_task.take()).await;
        stop
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
    use std::os::unix::process::ExitStatusExt;
    ExitDescription {
        code: status.code(),
        signal: status.signal(),
    }
}
