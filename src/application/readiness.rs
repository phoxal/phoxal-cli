use std::path::Path;
use std::process::{Child, ExitStatus};
use std::time::Duration;

use phoxal_cli_core::runtime::{ProcessState, ProjectLifecycle, StartupRequirement};
use phoxal_cli_protocol::SupervisorSnapshotV0;

pub(crate) trait StartupPresenter: Send {
    fn snapshot(&mut self, snapshot: &SupervisorSnapshotV0);
    fn tick(&mut self) {}
    fn ready(&mut self) {}
    fn cancelled(&mut self) {}
    fn failed(&mut self, _reason: Option<&str>, _log: &Path) {}
}

#[derive(Debug)]
pub(crate) enum StartupWait {
    Ready,
    Failed { reason: Option<String> },
    Cancelled,
    ChildExited { status: ExitStatus },
    FeedLost,
    DeadlineExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Readiness {
    Ready,
    Pending,
    Failed(Vec<String>),
}

pub(crate) fn required_readiness(snapshot: &SupervisorSnapshotV0) -> Readiness {
    match snapshot.lifecycle {
        ProjectLifecycle::Ready => Readiness::Ready,
        ProjectLifecycle::Degraded => {
            let required_failed = snapshot.processes.values().any(|entry| {
                entry.descriptor.startup_requirement == StartupRequirement::Required
                    && entry.status.actual == ProcessState::Failed
            });
            if required_failed {
                Readiness::Pending
            } else {
                Readiness::Ready
            }
        }
        ProjectLifecycle::Failed => Readiness::Failed(
            snapshot
                .processes
                .iter()
                .filter(|(_, entry)| entry.status.actual == ProcessState::Failed)
                .map(|(key, _)| key.to_string())
                .collect(),
        ),
        _ => Readiness::Pending,
    }
}

pub(crate) fn failure_reason(snapshot: &SupervisorSnapshotV0) -> Option<String> {
    snapshot
        .failure
        .clone()
        .or_else(|| match required_readiness(snapshot) {
            Readiness::Failed(failures) if !failures.is_empty() => {
                Some(format!("resident startup failed: {}", failures.join(", ")))
            }
            _ => None,
        })
}

fn terminal(snapshot: &SupervisorSnapshotV0) -> Option<StartupWait> {
    match required_readiness(snapshot) {
        Readiness::Ready => Some(StartupWait::Ready),
        Readiness::Pending => None,
        Readiness::Failed(_) => Some(StartupWait::Failed {
            reason: failure_reason(snapshot),
        }),
    }
}

const fn presentation_revision(snapshot: &SupervisorSnapshotV0) -> (u64, u64) {
    (snapshot.supervisor_generation, snapshot.revision)
}

pub(crate) async fn wait(
    feed: &phoxal_cli_client::SupervisorFeed,
    child: Option<&mut Child>,
    deadline: Option<tokio::time::Instant>,
    presenter: &mut dyn StartupPresenter,
) -> anyhow::Result<StartupWait> {
    wait_receiver(feed.subscribe(), child, deadline, presenter, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

async fn wait_receiver<I>(
    mut snapshots: tokio::sync::watch::Receiver<SupervisorSnapshotV0>,
    mut child: Option<&mut Child>,
    deadline: Option<tokio::time::Instant>,
    presenter: &mut dyn StartupPresenter,
    interrupt: I,
) -> anyhow::Result<StartupWait>
where
    I: std::future::Future<Output = ()>,
{
    tokio::pin!(interrupt);
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut presented_revision = None;
    loop {
        let snapshot = snapshots.borrow_and_update().clone();
        let revision = presentation_revision(&snapshot);
        if presented_revision != Some(revision) {
            presenter.snapshot(&snapshot);
            presented_revision = Some(revision);
        }
        if let Some(outcome) = terminal(&snapshot) {
            return Ok(outcome);
        }
        if let Some(child) = child.as_deref_mut()
            && let Some(status) = child.try_wait()?
        {
            return Ok(StartupWait::ChildExited { status });
        }

        tokio::select! {
            result = snapshots.changed() => {
                if result.is_err() {
                    return Ok(StartupWait::FeedLost);
                }
            }
            _ = ticker.tick() => presenter.tick(),
            _ = &mut interrupt => return Ok(StartupWait::Cancelled),
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => return Ok(StartupWait::DeadlineExceeded),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::runtime::{StartupStatus, StartupStep, StartupStepKind, StartupStepState};

    #[derive(Default)]
    struct Recorder(Vec<ProjectLifecycle>);

    impl StartupPresenter for Recorder {
        fn snapshot(&mut self, snapshot: &SupervisorSnapshotV0) {
            self.0.push(snapshot.lifecycle);
        }
    }

    #[test]
    fn a_failed_step_alone_does_not_end_startup() {
        let snapshot = SupervisorSnapshotV0 {
            lifecycle: ProjectLifecycle::Starting,
            startup: StartupStatus {
                steps: vec![StartupStep {
                    kind: StartupStepKind::PrepareRuntime,
                    state: StartupStepState::Failed,
                    detail: Some("failed first".to_string()),
                    elapsed_ms: Some(1),
                }],
            },
            ..SupervisorSnapshotV0::default()
        };
        assert!(terminal(&snapshot).is_none());
    }

    #[test]
    fn lifecycle_terminal_states_produce_typed_outcomes() {
        let ready = SupervisorSnapshotV0 {
            lifecycle: ProjectLifecycle::Ready,
            ..SupervisorSnapshotV0::default()
        };
        assert!(matches!(terminal(&ready), Some(StartupWait::Ready)));

        let failed = SupervisorSnapshotV0 {
            lifecycle: ProjectLifecycle::Failed,
            failure: Some("canonical reason".to_string()),
            ..SupervisorSnapshotV0::default()
        };
        assert!(matches!(
            terminal(&failed),
            Some(StartupWait::Failed { reason: Some(reason) }) if reason == "canonical reason"
        ));
    }

    #[test]
    fn presentation_revision_fences_a_reconnected_resident_generation() {
        let first = SupervisorSnapshotV0 {
            supervisor_generation: 1,
            revision: 7,
            ..SupervisorSnapshotV0::default()
        };
        let restarted = SupervisorSnapshotV0 {
            supervisor_generation: 2,
            revision: 7,
            ..SupervisorSnapshotV0::default()
        };
        assert_ne!(
            presentation_revision(&first),
            presentation_revision(&restarted)
        );
    }

    #[tokio::test]
    async fn controller_returns_ready_and_observes_failed_step_before_failed_lifecycle() {
        let mut starting = SupervisorSnapshotV0::default();
        starting.startup.steps.push(StartupStep {
            kind: StartupStepKind::PrepareRuntime,
            state: StartupStepState::Active,
            detail: None,
            elapsed_ms: None,
        });
        let (tx, rx) = tokio::sync::watch::channel(starting.clone());
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let mut step_failed = starting;
            step_failed.startup.steps[0].state = StartupStepState::Failed;
            step_failed.revision += 1;
            tx.send_replace(step_failed.clone());
            tokio::task::yield_now().await;
            step_failed.lifecycle = ProjectLifecycle::Failed;
            step_failed.failure = Some("canonical".to_string());
            step_failed.revision += 1;
            tx.send_replace(step_failed);
        });
        let mut presenter = Recorder::default();
        let outcome = wait_receiver(rx, None, None, &mut presenter, std::future::pending())
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            StartupWait::Failed { reason: Some(reason) } if reason == "canonical"
        ));
        assert!(presenter.0.len() >= 2);

        let ready = SupervisorSnapshotV0 {
            lifecycle: ProjectLifecycle::Ready,
            ..SupervisorSnapshotV0::default()
        };
        let (_tx, rx) = tokio::sync::watch::channel(ready);
        assert!(matches!(
            wait_receiver(
                rx,
                None,
                None,
                &mut Recorder::default(),
                std::future::pending()
            )
            .await
            .unwrap(),
            StartupWait::Ready
        ));
    }

    #[tokio::test]
    async fn controller_returns_cancel_feed_lost_deadline_and_child_exit() {
        let (_tx, rx) = tokio::sync::watch::channel(SupervisorSnapshotV0::default());
        assert!(matches!(
            wait_receiver(rx, None, None, &mut Recorder::default(), async {})
                .await
                .unwrap(),
            StartupWait::Cancelled
        ));

        let (tx, rx) = tokio::sync::watch::channel(SupervisorSnapshotV0::default());
        drop(tx);
        assert!(matches!(
            wait_receiver(
                rx,
                None,
                None,
                &mut Recorder::default(),
                std::future::pending()
            )
            .await
            .unwrap(),
            StartupWait::FeedLost
        ));

        let (_tx, rx) = tokio::sync::watch::channel(SupervisorSnapshotV0::default());
        assert!(matches!(
            wait_receiver(
                rx,
                None,
                Some(tokio::time::Instant::now()),
                &mut Recorder::default(),
                std::future::pending()
            )
            .await
            .unwrap(),
            StartupWait::DeadlineExceeded
        ));

        let (_tx, rx) = tokio::sync::watch::channel(SupervisorSnapshotV0::default());
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        assert!(matches!(
            wait_receiver(
                rx,
                Some(&mut child),
                Some(tokio::time::Instant::now() + Duration::from_secs(2)),
                &mut Recorder::default(),
                std::future::pending()
            )
            .await
            .unwrap(),
            StartupWait::ChildExited { status } if status.code() == Some(7)
        ));
    }
}
