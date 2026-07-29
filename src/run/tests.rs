use super::AbortTasks;
use anyhow::Result;
use phoxal_cli_core::project::launch_plan::{LaunchMode, LaunchPlan};
use phoxal_cli_core::runtime::ParticipantSpec;
use phoxal_cli_core::session::{
    ParticipantKind, ProcessKey, ReadinessPolicy, RuntimeFailurePolicy, StartupRequirement,
};
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn human_launch_report_enters_the_active_session_diagnostics() -> Result<()> {
    let _guard = crate::cli::output::diagnostics::DIAGNOSTICS_TEST_LOCK.blocking_lock();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    crate::cli::output::diagnostics::install(tx);
    let plan = LaunchPlan {
        mode: LaunchMode::Run,
        robots: Vec::new(),
    };
    let specs = [ParticipantSpec {
        key: ProcessKey::project("fixture"),
        id: "fixture".to_string(),
        kind: ParticipantKind::Tool,
        executable: PathBuf::from("fixture-command"),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        shutdown_grace: Duration::from_secs(1),
        process_group: false,
        note: None,
        bus_participant: false,
        readiness: ReadinessPolicy::ProcessSpawned,
        startup_requirement: StartupRequirement::Required,
        runtime_failure: RuntimeFailurePolicy::StopProject,
        restart_policy: Default::default(),
    }];

    let result = super::command::report_launch_commands(&plan, &specs, &crate::Ui::new(true));
    crate::cli::output::diagnostics::uninstall();
    result?;

    let messages = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            phoxal_cli_core::session::event::SessionEvent::Diagnostic { message, .. } => {
                Some(message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0], "resolved launch participants:");
    assert!(messages[1].contains("fixture (robot-tool) -> fixture-command"));
    assert!(messages[2].starts_with("motion guarantees:"));
    Ok(())
}

#[tokio::test]
async fn dropping_setup_background_tasks_aborts_every_handle() {
    let handle = tokio::spawn(std::future::pending::<()>());
    let abort = handle.abort_handle();
    let mut tasks = AbortTasks::default();
    tasks.push(handle);
    drop(tasks);
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
}
