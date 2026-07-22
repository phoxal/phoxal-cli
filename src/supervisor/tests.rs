//! Tests for this module.

use super::r#loop::{recv_action, request_participant_stop, shutdown_all};
use super::signals::process_group_alive;
use super::*;
use anyhow::{Context, Result};
use phoxal_cli_core::session::ParticipantKind;
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::session::output::WaitBudget;

#[test]
fn process_details_are_session_only_and_clear_with_a_restart() {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "motion",
        ParticipantKind::Service,
        ParticipantState::Ready,
    ));
    board.set_process_details("motion", Some(42), Some(1_024));

    let snapshot = board.snapshot();
    let status = snapshot.participants.get("motion").expect("motion");
    assert_eq!(status.pid, Some(42));
    assert_eq!(status.artifact_size_bytes, Some(1_024));
    let json = serde_json::to_value(&snapshot).expect("serialize board");
    let serialized = json.to_string();
    assert!(!serialized.contains("pid"));
    assert!(!serialized.contains("artifact_size_bytes"));

    board.set_state("motion", ParticipantState::Restarting, None);
    let snapshot = board.snapshot();
    let status = snapshot.participants.get("motion").expect("motion");
    assert_eq!(status.pid, None);
    assert_eq!(status.artifact_size_bytes, None);
}

#[test]
fn failed_status_keeps_a_live_pid_available_for_forced_shutdown() {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "motion",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    board.set_process_details("motion", Some(42), Some(1_024));

    board.set_state(
        "motion",
        ParticipantState::Failed,
        Some("readiness timed out while the process is still live".to_string()),
    );

    let snapshot = board.snapshot();
    let status = snapshot.participants.get("motion").expect("motion");
    assert_eq!(status.pid, Some(42));
    assert_eq!(status.artifact_size_bytes, Some(1_024));
}

fn spec(id: &str) -> ParticipantSpec {
    ParticipantSpec {
        key: phoxal_cli_core::session::ProcessKey::project(id),
        id: id.to_string(),
        kind: ParticipantKind::Service,
        executable: PathBuf::from("/bin/echo"),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        shutdown_grace: Duration::from_millis(10),
        process_group: false,
        note: None,
        bus_participant: true,
        readiness: ParticipantSpec::exact_liveliness_template(
            phoxal_cli_core::session::RobotKey::new("test", "robot"),
            id,
        ),
        startup_requirement: phoxal_cli_core::session::StartupRequirement::Required,
        runtime_failure: phoxal_cli_core::session::RuntimeFailurePolicy::StopProject,
        restart_policy: Default::default(),
    }
}

fn sleep_spec(id: &str) -> ParticipantSpec {
    ParticipantSpec {
        key: phoxal_cli_core::session::ProcessKey::project(id),
        id: id.to_string(),
        kind: ParticipantKind::Service,
        executable: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), "sleep 30".to_string()],
        cwd: None,
        env: Vec::new(),
        shutdown_grace: Duration::from_millis(50),
        process_group: false,
        note: None,
        bus_participant: true,
        readiness: ParticipantSpec::exact_liveliness_template(
            phoxal_cli_core::session::RobotKey::new("test", "robot"),
            id,
        ),
        startup_requirement: phoxal_cli_core::session::StartupRequirement::Required,
        runtime_failure: phoxal_cli_core::session::RuntimeFailurePolicy::StopProject,
        restart_policy: Default::default(),
    }
}

fn delayed_failure_spec(
    id: &str,
    policy: phoxal_cli_core::session::RuntimeFailurePolicy,
) -> ParticipantSpec {
    ParticipantSpec {
        executable: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), "sleep 0.2; kill -9 $$".to_string()],
        bus_participant: false,
        readiness: phoxal_cli_core::session::ReadinessPolicy::ProcessSpawned,
        runtime_failure: policy,
        restart_policy: RestartPolicy {
            restart_delay: Duration::from_millis(1),
            start_limit_interval: Duration::from_secs(60),
            start_limit_burst: 1,
        },
        ..spec(id)
    }
}

#[tokio::test]
async fn post_ready_stop_project_failure_terminates_the_project() -> Result<()> {
    let board = BoardBackend::new();
    let token = tokio_util::sync::CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        supervise_until_shutdown(
            vec![SupervisionStage::new(
                "graph",
                vec![delayed_failure_spec(
                    "mission",
                    phoxal_cli_core::session::RuntimeFailurePolicy::StopProject,
                )],
                WaitBudget::Bounded(Duration::from_secs(2)),
            )],
            board.clone(),
            SupervisorOptions {
                token,
                emits_running_on_startup_complete: true,
                ..SupervisorOptions::default()
            },
        ),
    )
    .await
    .expect("StopProject must terminate promptly")
    .expect_err("permanent failure must stop the project");
    assert!(result.to_string().contains("StopProject"));
    assert_eq!(
        board.supervisor_snapshot().lifecycle,
        phoxal_cli_core::session::ProjectLifecycle::Failed
    );
    Ok(())
}

#[tokio::test]
async fn post_ready_keep_degraded_failure_keeps_the_project_active() -> Result<()> {
    let board = BoardBackend::new();
    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        vec![SupervisionStage::new(
            "infrastructure",
            vec![delayed_failure_spec(
                "observer",
                phoxal_cli_core::session::RuntimeFailurePolicy::KeepProjectDegraded,
            )],
            WaitBudget::Bounded(Duration::from_secs(2)),
        )],
        board.clone(),
        SupervisorOptions {
            token: token.clone(),
            emits_running_on_startup_complete: true,
            ..SupervisorOptions::default()
        },
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while board.supervisor_snapshot().lifecycle
            != phoxal_cli_core::session::ProjectLifecycle::Degraded
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("KeepProjectDegraded must apply promptly");
    assert!(!supervise.is_finished());
    token.cancel();
    supervise.await.expect("supervisor task panicked")?;
    Ok(())
}

#[test]
fn orderly_shutdown_budget_covers_grace_group_reap_and_reader_joins() {
    let mut first = sleep_spec("first");
    first.shutdown_grace = Duration::from_secs(2);
    let mut second = sleep_spec("second");
    second.shutdown_grace = Duration::from_secs(3);
    let stages = vec![SupervisionStage::new(
        "all",
        vec![first, second],
        WaitBudget::Unbounded,
    )];

    assert_eq!(
        orderly_shutdown_budget(&stages),
        Duration::from_millis(5_500)
    );
}

#[tokio::test]
async fn shutdown_drains_concurrently_within_reverse_canonical_phases() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let log = temp.path().join("shutdown-order");
    let board = BoardBackend::new();
    let mut running = Vec::new();
    for (id, phase) in [
        ("infra", "starting project infrastructure"),
        ("graph-a", "starting robot graph"),
        ("graph-b", "starting robot graph"),
    ] {
        let mut spec = sleep_spec(id);
        spec.args = vec![
            "-c".to_string(),
            format!(
                "trap 'sleep 0.2; echo {id} >> {}; exit 0' TERM; while :; do :; done",
                log.display()
            ),
        ];
        spec.process_group = true;
        spec.shutdown_grace = Duration::from_secs(1);
        running.push(RunningParticipant::spawn_in_phase(spec, &board, phase).await?);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    shutdown_all(&mut running, &board).await;
    let elapsed = started.elapsed();
    let lines = std::fs::read_to_string(log)?
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(matches!(lines.as_slice(), [first, second, third]
        if (first == "graph-a" || first == "graph-b")
            && (second == "graph-a" || second == "graph-b")
            && first != second
            && third == "infra"));
    assert!(
        elapsed < Duration::from_millis(750),
        "shutdown was not concurrent: {elapsed:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn ordinary_stop_kills_the_entire_isolated_process_group() -> Result<()> {
    let mut grouped = sleep_spec("grouped");
    grouped.process_group = true;
    grouped.shutdown_grace = Duration::from_millis(50);
    grouped.args = vec![
        "-c".to_string(),
        "trap '' TERM; sleep 30 & wait".to_string(),
    ];
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "grouped",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let mut participant = RunningParticipant::spawn(grouped, &board).await?;
    let pid = participant
        .child
        .as_ref()
        .and_then(|child| child.id())
        .context("group leader should have a pid")?;

    participant.stop_current(&board).await?;

    assert!(!process_group_alive(pid)?);
    assert_eq!(board.snapshot().participants["grouped"].pid, None);
    Ok(())
}

#[tokio::test]
async fn closed_action_receiver_is_consumed_once_then_stays_pending() {
    let (action_tx, action_rx) = mpsc::channel(1);
    drop(action_tx);
    let action_rx = SupervisorActionReceiver::new(action_rx);

    assert!(recv_action(Some(&action_rx)).await.is_none());

    // A closed receiver is terminal, not a stream of immediate `None`s;
    // otherwise it would win every supervisor `select!` pass and spin.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), recv_action(Some(&action_rx)))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn recovery_epoch_resets_reconcilers_and_preserves_webots_ownership() {
    let board = BoardBackend::new();
    let mut log_reconciler = board.recovery_epoch_receiver();
    let mut bus_reconciler = board.recovery_epoch_receiver();
    let mut webots =
        ParticipantStatus::new("webots", ParticipantKind::Tool, ParticipantState::Ready);
    webots.note = Some("CLI-managed Webots application".to_string());
    webots.pid = Some(41);
    webots.restart_count = 2;
    board.upsert(webots);
    let mut controller = ParticipantStatus::new(
        "simulator-webots-controller-robot",
        ParticipantKind::Simulator,
        ParticipantState::Ready,
    );
    controller.note = Some("SimulationManaged: launched by Webots".to_string());
    board.upsert(controller);
    board.record_presence("webots", true);
    board.record_presence("simulator-webots-controller-robot", true);

    let epoch = board.begin_recovery_epoch(
        &[(
            phoxal_cli_core::session::ProcessKey::project("webots"),
            Some("CLI-managed Webots application".to_string()),
        )],
        &[phoxal_cli_core::session::ProcessKey::project(
            "simulator-webots-controller-robot",
        )],
    );

    assert_eq!(epoch, 1);
    assert_eq!(board.recovery_epoch(), 1);
    tokio::time::timeout(Duration::from_millis(20), log_reconciler.changed())
        .await
        .expect("log reconciler reset must be prompt")
        .expect("board retains the reset sender");
    tokio::time::timeout(Duration::from_millis(20), bus_reconciler.changed())
        .await
        .expect("bus reconciler reset must be prompt")
        .expect("board retains the reset sender");
    assert_eq!(*log_reconciler.borrow_and_update(), epoch);
    assert_eq!(*bus_reconciler.borrow_and_update(), epoch);
    assert!(!board.is_present("webots"));
    assert!(!board.is_present("simulator-webots-controller-robot"));
    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants["webots"].state,
        ParticipantState::Starting
    );
    assert_eq!(snapshot.participants["webots"].pid, None);
    assert_eq!(snapshot.participants["webots"].restart_count, 0);
    assert_eq!(
        snapshot.participants["simulator-webots-controller-robot"]
            .note
            .as_deref(),
        Some("SimulationManaged: launched by Webots")
    );

    board.record_presence("simulator-webots-controller-robot", true);
    assert_eq!(
        board.snapshot().participants["simulator-webots-controller-robot"].state,
        ParticipantState::Starting,
        "observations from the dead router must be fenced"
    );
    board.enable_presence_for_recovery();
    board.record_presence("simulator-webots-controller-robot", true);
    assert_eq!(
        board.snapshot().participants["simulator-webots-controller-robot"].state,
        ParticipantState::Ready
    );
}

#[test]
fn launch_command_prints_contract_flags_and_env() {
    let mut spec = spec("mission");
    spec.executable = PathBuf::from("/tmp/phoxal mission");
    spec.env = vec![
        (
            phoxal::participant::launch::env::PARTICIPANT_ID.to_string(),
            "mission".to_string(),
        ),
        (
            phoxal::participant::launch::env::ROBOT_ID.to_string(),
            "robot".to_string(),
        ),
        (
            phoxal::participant::launch::env::CONNECT.to_string(),
            "tcp/localhost:7447".to_string(),
        ),
    ];

    let launch = spec.launch_command();
    assert!(
        launch
            .command_line
            .contains("'/tmp/phoxal mission' --participant-id mission"),
        "{}",
        launch.command_line
    );
    assert!(
        launch.command_line.contains("--connect tcp/localhost:7447"),
        "{}",
        launch.command_line
    );
    assert_eq!(
        launch
            .env
            .get(phoxal::participant::launch::env::PARTICIPANT_ID)
            .map(String::as_str),
        Some("mission")
    );
}

#[tokio::test]
async fn requested_webots_sigterm_exit_is_stopped_not_failed() -> Result<()> {
    let mut webots = sleep_spec("webots");
    webots.args = vec![
        "-c".to_string(),
        "trap 'exit 0' TERM; while :; do sleep 1; done".to_string(),
    ];
    webots.process_group = true;

    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "webots",
        ParticipantKind::Tool,
        ParticipantState::Starting,
    ));
    let participant = RunningParticipant::spawn(webots, &board).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut running = vec![participant];

    request_participant_stop(
        &mut running,
        &board,
        RequestedStop::new("webots", Duration::from_secs(1)),
    )
    .await;

    let snapshot = board.snapshot();
    let status = snapshot.participants.get("webots").expect("webots");
    assert_eq!(status.state, ParticipantState::Stopped);
    assert!(snapshot.failed_participants().is_empty());
    assert!(!running[0].is_active());
    Ok(())
}

#[tokio::test]
async fn webots_crash_reaped_during_requested_stop_is_failed() -> Result<()> {
    let mut webots = sleep_spec("webots");
    webots.args = vec!["-c".to_string(), "exit 7".to_string()];
    webots.process_group = true;

    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "webots",
        ParticipantKind::Tool,
        ParticipantState::Starting,
    ));
    let participant = RunningParticipant::spawn(webots, &board).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut running = vec![participant];

    request_participant_stop(
        &mut running,
        &board,
        RequestedStop::new("webots", Duration::from_secs(1)),
    )
    .await;

    let snapshot = board.snapshot();
    let status = snapshot.participants.get("webots").expect("webots");
    assert_eq!(status.state, ParticipantState::Failed);
    let outcome = SupervisorOutcome {
        failed_participants: snapshot.failed_participants(),
    };
    assert!(!outcome.graph_healthy());
    assert_eq!(outcome.failed_participants, vec!["webots"]);
    assert!(!running[0].is_active());
    Ok(())
}

#[tokio::test]
async fn webots_crash_already_waiting_to_restart_is_failed_at_stop() -> Result<()> {
    let mut webots = sleep_spec("webots");
    webots.args = vec!["-c".to_string(), "exit 7".to_string()];
    webots.process_group = true;

    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "webots",
        ParticipantKind::Tool,
        ParticipantState::Starting,
    ));
    let mut participant = RunningParticipant::spawn(webots, &board).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    participant
        .poll(
            &board,
            &RestartPolicy {
                restart_delay: Duration::from_secs(30),
                ..RestartPolicy::default()
            },
        )
        .await?;
    assert!(participant.child.is_none());
    assert!(participant.restart_at.is_some());
    let mut running = vec![participant];

    request_participant_stop(
        &mut running,
        &board,
        RequestedStop::new("webots", Duration::from_secs(1)),
    )
    .await;

    let snapshot = board.snapshot();
    let status = snapshot.participants.get("webots").expect("webots");
    assert_eq!(status.state, ParticipantState::Failed);
    assert_eq!(snapshot.failed_participants(), vec!["webots"]);
    assert!(running[0].restart_at.is_none());
    assert!(!running[0].is_active());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn requested_webots_stop_uses_sigkill_only_after_term_grace() -> Result<()> {
    let mut webots = sleep_spec("webots");
    webots.args = vec!["-c".to_string(), "trap '' TERM; sleep 30".to_string()];
    webots.process_group = true;
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "webots",
        ParticipantKind::Tool,
        ParticipantState::Starting,
    ));
    let participant = RunningParticipant::spawn(webots, &board).await?;
    let pid = participant
        .child
        .as_ref()
        .and_then(|child| child.id())
        .context("test child has no pid")?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut running = vec![participant];

    request_participant_stop(
        &mut running,
        &board,
        RequestedStop::new("webots", Duration::from_millis(20)),
    )
    .await;

    assert!(!process_group_alive(pid)?);
    let snapshot = board.snapshot();
    let status = snapshot.participants.get("webots").expect("webots");
    assert_eq!(status.state, ParticipantState::Failed);
    assert!(
        status
            .note
            .as_deref()
            .is_some_and(|note| note.contains("SIGKILL fallback"))
    );
    Ok(())
}

#[tokio::test]
async fn watch_swap_does_not_consume_restart_budget() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;
    participant.restart_count = 4;
    participant
        .failure_times
        .push_back(Instant::now() - Duration::from_secs(1));

    participant
        .swap(
            sleep_spec("mission"),
            &board,
            "ok 0.1s, restarted".to_string(),
        )
        .await?;

    assert_eq!(participant.restart_count, 4);
    assert_eq!(participant.failure_times.len(), 1);
    let snapshot = board.snapshot();
    let status = snapshot.participants.get("mission").expect("mission");
    // OBSERVED readiness: swap lands back at `Starting` (this fixture
    // never appears in Liveliness), but the swap note is still attached immediately.
    assert_eq!(status.state, ParticipantState::Starting);
    assert_eq!(status.note.as_deref(), Some("ok 0.1s, restarted"));

    participant.stop_current(&board).await?;
    Ok(())
}

#[tokio::test]
async fn restart_spawn_failure_is_participant_status_not_supervisor_error() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;
    participant.stop_current(&board).await?;
    participant.spec.executable = PathBuf::from("/definitely/missing/phoxal-participant");
    participant.restart_at = Some(Instant::now());

    participant.poll(&board, &RestartPolicy::default()).await?;

    assert!(participant.failed);
    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants["mission"].state,
        ParticipantState::Failed
    );
    assert!(
        snapshot.participants["mission"]
            .note
            .as_deref()
            .is_some_and(|note| note.contains("restart spawn failed"))
    );
    Ok(())
}

#[tokio::test]
async fn swap_spawn_failure_is_participant_status_not_supervisor_error() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;
    let mut missing = sleep_spec("mission");
    missing.executable = PathBuf::from("/definitely/missing/phoxal-participant");

    participant
        .swap(missing, &board, "watch rebuild completed".to_string())
        .await?;

    assert!(participant.failed);
    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants["mission"].state,
        ParticipantState::Failed
    );
    assert!(
        snapshot.participants["mission"]
            .note
            .as_deref()
            .is_some_and(|note| note.contains("swap spawn failed"))
    );
    Ok(())
}

#[tokio::test]
async fn optional_startup_failure_degrades_without_ending_the_session() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "flap",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let flappy = ParticipantSpec {
        executable: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), "exit 7".to_string()],
        startup_requirement: phoxal_cli_core::session::StartupRequirement::Optional,
        runtime_failure: phoxal_cli_core::session::RuntimeFailurePolicy::KeepProjectDegraded,
        restart_policy: RestartPolicy {
            restart_delay: Duration::from_millis(1),
            start_limit_interval: Duration::from_secs(60),
            start_limit_burst: 1,
        },
        ..spec("flap")
    };
    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        vec![SupervisionStage::new(
            "stage",
            vec![flappy],
            WaitBudget::Bounded(Duration::from_secs(5)),
        )],
        board.clone(),
        SupervisorOptions {
            token: token.clone(),
            emits_running_on_startup_complete: true,
            ..SupervisorOptions::default()
        },
    ));

    tokio::time::timeout(Duration::from_secs(5), async {
        while board.snapshot().participants["flap"].state != ParticipantState::Failed {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the failed child must remain visible on the board");
    assert!(
        board.snapshot().participants["flap"]
            .note
            .as_deref()
            .is_some_and(|note| note.contains("StartLimitBurst")),
        "restart exhaustion must explain the terminal participant status"
    );
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(
        !supervise.is_finished(),
        "an optional failure must leave the degraded project active"
    );
    assert_eq!(
        board.supervisor_snapshot().lifecycle,
        phoxal_cli_core::session::ProjectLifecycle::Degraded
    );

    token.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(3), supervise)
        .await
        .expect("user stop must tear down promptly")
        .expect("supervisor task panicked")?;
    assert_eq!(outcome.failed_participants, vec!["flap"]);
    Ok(())
}

#[tokio::test]
async fn participant_failure_stays_visible_until_user_stop() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "webots",
        ParticipantKind::Tool,
        ParticipantState::Starting,
    ));
    board.upsert(ParticipantStatus::new(
        "simulator-webots-controller-robot",
        ParticipantKind::Service,
        ParticipantState::Ready,
    ));

    let mut webots = sleep_spec("webots");
    webots.kind = ParticipantKind::Tool;
    webots.args = vec![
        "-c".to_string(),
        "trap 'exit 0' TERM; while :; do sleep 1; done".to_string(),
    ];
    webots.process_group = true;
    webots.bus_participant = false;

    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        vec![SupervisionStage::new(
            "stage",
            vec![webots],
            WaitBudget::Bounded(Duration::from_secs(5)),
        )],
        board.clone(),
        SupervisorOptions {
            requested_stop: Some(RequestedStop::new("webots", Duration::from_secs(1))),
            token: token.clone(),
            ..SupervisorOptions::default()
        },
    ));

    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(
        !supervise.is_finished(),
        "the session must remain up until the user stops it"
    );
    board.set_state(
        "simulator-webots-controller-robot",
        ParticipantState::Failed,
        Some("controller reported terminal failure".to_string()),
    );

    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(
        !supervise.is_finished(),
        "a failed participant must not terminate the session"
    );

    token.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(3), supervise)
        .await
        .expect("user stop must tear down promptly")
        .expect("supervisor task panicked")?;
    assert!(!outcome.graph_healthy());
    assert_eq!(
        outcome.failed_participants,
        vec!["simulator-webots-controller-robot"]
    );
    assert_eq!(
        board.snapshot().participants["webots"].state,
        ParticipantState::Stopped
    );
    Ok(())
}

#[tokio::test]
async fn local_log_capture_works_without_bus() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "logger",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let specs = vec![ParticipantSpec {
        executable: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), "echo local-line; exit 2".to_string()],
        restart_policy: RestartPolicy {
            restart_delay: Duration::from_millis(1),
            start_limit_interval: Duration::from_secs(1),
            start_limit_burst: 1,
        },
        ..spec("logger")
    }];
    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        vec![SupervisionStage::new(
            "stage",
            specs,
            WaitBudget::Bounded(Duration::from_secs(5)),
        )],
        board.clone(),
        SupervisorOptions {
            action_rx: None,
            requested_stop: None,
            token: token.clone(),
            events: None,
            emits_running_on_startup_complete: false,
        },
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while board.snapshot().participants["logger"].state != ParticipantState::Failed {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the logger exits before readiness and must fail its stage");

    let snapshot = board.snapshot();
    let status = snapshot.participants.get("logger").expect("logger status");
    assert!(
        status
            .last_log_lines
            .iter()
            .any(|line| line.contains("stdout: local-line")),
        "{status:?}"
    );
    let error = supervise
        .await
        .expect("supervisor task panicked")
        .expect_err("required pre-readiness crash must fail startup");
    assert!(error.to_string().contains("stage"));
    Ok(())
}

#[test]
fn spawn_no_longer_marks_a_bus_participant_ready() {
    // The core observed-readiness invariant: `bus_participant: true` (the
    // default for every real phoxal participant, see the field docs) must
    // stay `Starting` through a successful spawn - `Ready` now comes only
    // from an observed Liveliness appearance (`BoardBackend::record_presence`).
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants["mission"].state,
        ParticipantState::Starting
    );

    board.record_presence("mission", true);
    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants["mission"].state,
        ParticipantState::Ready
    );
}

#[test]
fn liveliness_from_an_unplanned_participant_is_ignored() {
    let board = BoardBackend::new();
    board.record_presence("unplanned", true);
    assert!(board.snapshot().participants.is_empty());
}

#[test]
fn bus_log_from_an_unplanned_participant_is_ignored() {
    let board = BoardBackend::new();
    let (sender, mut receiver) = mpsc::channel(1);
    board.set_log_sink(sender);

    board.route_log_line(RoutedLogLine {
        participant: "\u{1b}[2Junplanned".to_string(),
        source: LogSource::Bus,
        severity: LogSeverity::Warn,
        text: "forged runtime row".to_string(),
        event_time: std::time::SystemTime::UNIX_EPOCH,
        scope: Some(LogScope {
            namespace: "acme".to_string(),
            robot_id: "r1".to_string(),
        }),
    });

    assert!(board.snapshot().participants.is_empty());
    assert!(receiver.try_recv().is_err());
}

#[test]
fn routed_log_text_is_bounded_before_board_retention() {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "motion",
        ParticipantKind::Service,
        ParticipantState::Ready,
    ));
    board.route_log_line(RoutedLogLine {
        participant: "motion".to_string(),
        source: LogSource::Bus,
        severity: LogSeverity::Info,
        text: "x".repeat(MAX_LOG_TEXT_CHARS * 2),
        event_time: std::time::SystemTime::UNIX_EPOCH,
        scope: Some(LogScope {
            namespace: "acme".to_string(),
            robot_id: "r1".to_string(),
        }),
    });
    let retained = board.snapshot().participants["motion"]
        .last_log_line
        .clone()
        .expect("retained line");
    assert_eq!(retained.chars().count(), MAX_LOG_TEXT_CHARS + 1);
    assert!(retained.ends_with('…'));
}

#[tokio::test]
async fn captured_newline_free_output_is_bounded_while_it_is_read() {
    use tokio::io::AsyncWriteExt;

    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "noisy",
        ParticipantKind::Service,
        ParticipantState::Ready,
    ));
    let (reader, mut writer) = tokio::io::duplex(1_024);
    let task = spawn_output_reader(board.clone(), "noisy".to_string(), "stdout", reader);
    writer
        .write_all(&vec![b'x'; MAX_CAPTURED_LINE_BYTES * 8])
        .await
        .expect("write oversized line");
    drop(writer);
    task.await.expect("output reader task");

    let retained = board.snapshot().participants["noisy"]
        .last_log_line
        .clone()
        .expect("retained line");
    assert_eq!(retained.chars().count(), MAX_LOG_TEXT_CHARS + 1);
    assert!(retained.ends_with('…'));
}

#[test]
fn presence_loss_is_observational_and_can_recover() {
    let id = "simulator-webots-controller-robot-v1";
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        id,
        ParticipantKind::Simulator,
        ParticipantState::Starting,
    ));
    board.record_presence(id, true);
    board.record_presence(id, false);

    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants[id].state,
        ParticipantState::Ready,
        "losing presence is observable, not process failure authority"
    );
    assert_eq!(snapshot.participants[id].present, Some(false));
    assert!(snapshot.participants[id].note.is_none());

    board.record_presence(id, true);
    let snapshot = board.snapshot();
    assert_eq!(snapshot.participants[id].state, ParticipantState::Ready);
    assert!(snapshot.participants[id].note.is_none());
}

#[test]
fn first_presence_preserves_launch_context_note() {
    let id = "simulator-webots-controller-robot-v1";
    let board = BoardBackend::new();
    let mut status =
        ParticipantStatus::new(id, ParticipantKind::Simulator, ParticipantState::Starting);
    status.note = Some("SimulationManaged: launched by Webots".to_string());
    board.upsert(status);

    board.record_presence(id, true);

    let snapshot = board.snapshot();
    assert_eq!(snapshot.participants[id].state, ParticipantState::Ready);
    assert_eq!(
        snapshot.participants[id].note.as_deref(),
        Some("SimulationManaged: launched by Webots")
    );
}

#[test]
fn lost_before_first_appearance_keeps_starting_state() {
    let id = "slow-setup";
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        id,
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    board.record_presence(id, false);

    let snapshot = board.snapshot();
    assert_eq!(snapshot.participants[id].state, ParticipantState::Starting);
}

#[tokio::test]
async fn respawn_requires_the_new_exact_incarnation_despite_stable_presence() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let mut participant = RunningParticipant::spawn(sleep_spec("mission"), &board).await?;
    let first_incarnation = board.supervisor_snapshot().processes
        [&phoxal_cli_core::session::ProcessKey::project("mission")]
        .status
        .incarnation
        .expect("the first spawn mints an incarnation");
    let robot = phoxal_cli_core::session::RobotKey::new("test", "robot");
    board.record_instance_presence(
        phoxal_cli_core::session::ParticipantInstanceKey {
            robot: robot.clone(),
            participant: "mission".to_string(),
            incarnation: first_incarnation,
        },
        true,
    );
    assert_eq!(
        board.snapshot().participants["mission"].state,
        ParticipantState::Ready
    );

    participant
        .child
        .as_mut()
        .expect("spawned child")
        .start_kill()?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let restart_policy = RestartPolicy {
        restart_delay: Duration::from_millis(1),
        start_limit_interval: Duration::from_secs(60),
        start_limit_burst: 3,
    };
    participant.poll(&board, &restart_policy).await?;
    assert_eq!(
        board.supervisor_snapshot().processes
            [&phoxal_cli_core::session::ProcessKey::project("mission")]
            .status
            .incarnation,
        Some(first_incarnation),
        "the failed row keeps its incarnation until replacement spawn"
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    participant.poll(&board, &restart_policy).await?;
    let second_incarnation = board.supervisor_snapshot().processes
        [&phoxal_cli_core::session::ProcessKey::project("mission")]
        .status
        .incarnation
        .expect("the replacement spawn mints an incarnation");
    assert_ne!(first_incarnation, second_incarnation);

    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants["mission"].state,
        ParticipantState::Starting,
        "a stale stable-key holder must not satisfy the replacement incarnation"
    );
    board.record_instance_presence(
        phoxal_cli_core::session::ParticipantInstanceKey {
            robot,
            participant: "mission".to_string(),
            incarnation: second_incarnation,
        },
        true,
    );
    assert_eq!(
        board.snapshot().participants["mission"].state,
        ParticipantState::Ready,
        "only the replacement incarnation may satisfy readiness"
    );

    participant.stop_current(&board).await?;
    Ok(())
}

#[test]
fn liveliness_cannot_resurrect_a_terminal_participant() {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Failed,
    ));
    // An Alive event that was in flight when the process independently died
    // must not undo the failure the process supervisor already recorded.
    board.record_presence("mission", true);
    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants["mission"].state,
        ParticipantState::Failed
    );
}

/// A simulation-managed participant (the Webots supervisor/controller: no
/// `ParticipantSpec`, no supervised process, launched by Webots itself) that
/// never appears in Liveliness must both (a) make participant readiness fail with a
/// clear, bounded-time error instead of hanging, and (b) be counted as
/// failed afterward so `SupervisorOutcome::graph_healthy` reflects it even
/// though no process crash was ever observed.
#[tokio::test]
async fn participant_wait_times_out_on_a_simulation_managed_participant_that_never_appears() {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "simulator-webots-supervisor",
        ParticipantKind::Tool,
        ParticipantState::Starting,
    ));
    board.upsert(ParticipantStatus::new(
        "simulator-webots-controller-robot",
        ParticipantKind::Tool,
        ParticipantState::Starting,
    ));
    // The supervisor checks in...
    board.record_presence("simulator-webots-supervisor", true);
    // ...but the controller never does.

    let expected = vec![
        "simulator-webots-supervisor".to_string(),
        "simulator-webots-controller-robot".to_string(),
    ];
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        await_participants_ready(
            &board,
            &expected,
            WaitBudget::Bounded(Duration::from_millis(300)),
            Duration::from_millis(20),
        ),
    )
    .await
    .expect("participant wait must return within its own timeout, never hang");

    let error = result.expect_err("a controller that never appears must fail the barrier");
    assert!(
        error
            .to_string()
            .contains("simulator-webots-controller-robot"),
        "error should name the missing participant: {error}"
    );

    // Failure propagation: the wait's own board-marking side
    // effect is what makes the graph unhealthy, even though this
    // participant never had a supervised process to crash.
    let snapshot = board.snapshot();
    assert_eq!(
        snapshot.participants["simulator-webots-controller-robot"].state,
        ParticipantState::Failed
    );
    assert_eq!(
        snapshot.participants["simulator-webots-supervisor"].state,
        ParticipantState::Ready,
        "the participant that DID appear must not be dragged down by the other's timeout"
    );
    let outcome = SupervisorOutcome {
        failed_participants: snapshot.failed_participants(),
    };
    assert!(!outcome.graph_healthy());
    assert_eq!(
        outcome.failed_participants,
        vec!["simulator-webots-controller-robot"]
    );
}

#[tokio::test]
async fn await_participants_ready_succeeds_once_everything_is_observed() {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    board.record_presence("mission", true);

    await_participants_ready(
        &board,
        &["mission".to_string()],
        WaitBudget::Bounded(Duration::from_secs(5)),
        Duration::from_millis(10),
    )
    .await
    .expect("everything is ready; must not error");
}

#[tokio::test]
async fn await_participants_ready_times_out_and_marks_missing_failed() {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        await_participants_ready(
            &board,
            &["mission".to_string()],
            WaitBudget::Bounded(Duration::from_millis(150)),
            Duration::from_millis(10),
        ),
    )
    .await
    .expect("must return within its own timeout, never hang")
    .expect_err("a participant that never appears must fail the wait");

    assert!(
        error.to_string().contains("mission"),
        "error should name the missing participant: {error}"
    );
    assert_eq!(
        board.snapshot().participants["mission"].state,
        ParticipantState::Failed
    );
}

#[tokio::test]
async fn await_participants_ready_fails_immediately_on_explicit_terminal_failure() {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "mission",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    board.set_state(
        "mission",
        ParticipantState::Failed,
        Some("process exited before readiness".to_string()),
    );

    let error = tokio::time::timeout(
        Duration::from_millis(200),
        await_participants_ready(
            &board,
            &["mission".to_string()],
            WaitBudget::Bounded(Duration::from_secs(60)),
            Duration::from_millis(10),
        ),
    )
    .await
    .expect("direct process failure must bypass the stage timeout")
    .expect_err("direct process failure must fail the wait");

    assert_eq!(
        error.to_string(),
        "stage ended unhealthy; failed participants: mission"
    );
}

/// The core staged-startup acceptance: nothing in a later stage spawns
/// until the previous stage is OBSERVED ready (not merely spawned).
/// "two" (`sleep_spec`, `bus_participant: true`) never appears in Liveliness
/// from this test until it manually sends one, so its absence from the
/// board proves stage two has not spawned yet.
#[tokio::test]
async fn staged_startup_gates_the_next_stage_on_observed_readiness() -> Result<()> {
    let board = BoardBackend::new();
    // Every real caller (`prepare_site_tools`/`prepare_robot_participants`)
    // upserts a `Starting` board entry BEFORE a spec ever reaches the
    // supervisor. Mirror that real contract for "one" only - "two"
    // deliberately stays un-upserted, so its absence from the board is
    // this test's proof that stage two has not spawned yet.
    board.upsert(ParticipantStatus::new(
        "one",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let stages = vec![
        SupervisionStage::new(
            "stage-one",
            vec![sleep_spec("one")],
            WaitBudget::Bounded(Duration::from_secs(5)),
        ),
        SupervisionStage::new(
            "stage-two",
            vec![sleep_spec("two")],
            WaitBudget::Bounded(Duration::from_secs(5)),
        ),
    ];
    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        stages,
        board.clone(),
        SupervisorOptions {
            token: token.clone(),
            ..SupervisorOptions::default()
        },
    ));

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !board.snapshot().participants.contains_key("two"),
        "stage two must not spawn until stage one is observed ready"
    );

    board.record_presence("one", true);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !board.snapshot().participants.contains_key("two") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stage two must spawn once stage one is observed ready");

    token.cancel();
    tokio::time::timeout(Duration::from_secs(3), supervise)
        .await
        .expect("supervisor must exit promptly")
        .expect("supervisor task panicked")?;
    Ok(())
}

/// A crashing member of the CURRENT (still-waiting) stage must keep
/// restarting while that stage's own readiness wait is pending - the
/// wait runs as one branch of the same `select!` as poll/restart, not
/// inline before it, so it can never freeze supervision.
#[tokio::test]
async fn restart_still_happens_while_a_stage_readiness_wait_is_pending() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "flap",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let flappy = ParticipantSpec {
        executable: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), "exit 7".to_string()],
        // Never appears in Liveliness, so this stage's own wait never completes -
        // the whole observation window below runs with `pending_stage`
        // `Some`, proving restart still happens during that wait.
        bus_participant: true,
        restart_policy: RestartPolicy {
            restart_delay: Duration::from_millis(20),
            start_limit_interval: Duration::from_secs(60),
            start_limit_burst: 1000,
        },
        ..spec("flap")
    };
    let stages = vec![SupervisionStage::new(
        "stage-one",
        vec![flappy],
        WaitBudget::Bounded(Duration::from_secs(30)),
    )];
    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        stages,
        board.clone(),
        SupervisorOptions {
            token: token.clone(),
            ..SupervisorOptions::default()
        },
    ));

    // Poll/restart only happen on the supervisor's 500ms ticker branch,
    // so the observation window must span several ticks to prove
    // restarts keep accumulating - too short a window would just prove
    // "no crash happened yet", not "restart runs concurrently with the
    // wait".
    tokio::time::sleep(Duration::from_millis(1700)).await;
    let restart_count = board.snapshot().participants["flap"].restart_count;
    assert!(
        restart_count >= 2,
        "expected multiple restarts while the stage-one readiness wait was still pending, got {restart_count}"
    );
    // "flap" is mid crash-loop, so its state at this exact instant is
    // either `Starting` (just respawned) or `Restarting` (waiting out
    // `restart_delay`) - never `Ready`, since it never appears.
    assert!(
        matches!(
            board.snapshot().participants["flap"].state,
            ParticipantState::Starting | ParticipantState::Restarting
        ),
        "flap never appears, so stage one's own wait must still be pending: {:?}",
        board.snapshot().participants["flap"].state
    );

    token.cancel();
    tokio::time::timeout(Duration::from_secs(3), supervise)
        .await
        .expect("supervisor must exit promptly")
        .expect("supervisor task panicked")?;
    Ok(())
}

/// Cancelling the session token WHILE a stage readiness wait is still
/// pending must still
/// break the loop and tear down promptly, proving the wait branch never
/// starves the other `select!` branches.
#[tokio::test]
async fn cancellation_during_a_stage_wait_still_tears_down_promptly() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "one",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let stages = vec![SupervisionStage::new(
        "stage-one",
        vec![sleep_spec("one")],
        WaitBudget::Bounded(Duration::from_secs(30)),
    )];
    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        stages,
        board.clone(),
        SupervisorOptions {
            token: token.clone(),
            ..SupervisorOptions::default()
        },
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        board.snapshot().participants["one"].state,
        ParticipantState::Starting,
        "stage one must still be waiting on its own readiness"
    );

    token.cancel();
    // The proof is promptness itself: `supervise_until_shutdown` must
    // return well inside the timeout even though "one"'s own stage wait
    // was still pending when the cancel arrived - a plain (non-requested)
    // teardown via `shutdown_all` stops the child but does not relabel
    // its board state (that relabeling is `RequestedStop`-specific, see
    // `request_participant_stop`).
    tokio::time::timeout(Duration::from_secs(3), supervise)
        .await
        .expect("supervisor must exit promptly even mid-stage-wait")
        .expect("supervisor task panicked")?;
    Ok(())
}

/// A required stage that never reaches readiness fails the project.
#[tokio::test]
async fn stalled_stage_times_out_and_marks_missing_participants_failed() -> Result<()> {
    let board = BoardBackend::new();
    for id in ["one", "later"] {
        board.upsert(ParticipantStatus::new(
            id,
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
    }
    let mut later = sleep_spec("later");
    later.bus_participant = false;
    let stages = vec![
        SupervisionStage::new(
            "stage-one",
            vec![sleep_spec("one")],
            WaitBudget::Bounded(Duration::from_millis(150)),
        ),
        SupervisionStage::new(
            "stage-two",
            vec![later],
            WaitBudget::Bounded(Duration::from_secs(2)),
        ),
    ];
    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        stages,
        board.clone(),
        SupervisorOptions {
            token: token.clone(),
            ..SupervisorOptions::default()
        },
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while board.snapshot().participants["one"].state != ParticipantState::Failed {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a stalled stage must be marked within its own timeout");

    assert_eq!(board.snapshot().failed_participants(), vec!["one"]);
    let error = supervise
        .await
        .expect("supervisor task panicked")
        .expect_err("required startup timeout must fail the project");
    assert!(error.to_string().contains("stage-one"));
    assert_eq!(
        board.snapshot().participants["later"].pid,
        None,
        "a hung required process must block every later phase"
    );
    assert_eq!(
        board.supervisor_snapshot().lifecycle,
        phoxal_cli_core::session::ProjectLifecycle::Failed
    );
    Ok(())
}

#[tokio::test]
async fn failed_required_stage_blocks_later_stage_and_ends_failed() -> Result<()> {
    let board = BoardBackend::new();
    for id in ["broken", "later"] {
        board.upsert(ParticipantStatus::new(
            id,
            ParticipantKind::Service,
            ParticipantState::Starting,
        ));
    }
    let mut broken = sleep_spec("broken");
    broken.executable = PathBuf::from("/definitely/missing/phoxal-participant");
    let mut later = sleep_spec("later");
    later.bus_participant = false;
    let token = tokio_util::sync::CancellationToken::new();
    let supervise = tokio::spawn(supervise_until_shutdown(
        vec![
            SupervisionStage::new(
                "broken-stage",
                vec![broken],
                WaitBudget::Bounded(Duration::from_secs(2)),
            ),
            SupervisionStage::new(
                "later-stage",
                vec![later],
                WaitBudget::Bounded(Duration::from_secs(2)),
            ),
        ],
        board.clone(),
        SupervisorOptions {
            token: token.clone(),
            ..SupervisorOptions::default()
        },
    ));

    let error = tokio::time::timeout(Duration::from_secs(5), supervise)
        .await
        .expect("required failure must terminate promptly")
        .expect("supervisor task panicked")
        .expect_err("required failure must end the project");
    assert!(error.to_string().contains("broken-stage"));
    let later = &board.snapshot().participants["later"];
    assert_eq!(later.state, ParticipantState::Starting);
    assert_eq!(later.pid, None, "the later stage must never spawn");
    assert_eq!(
        board.supervisor_snapshot().lifecycle,
        phoxal_cli_core::session::ProjectLifecycle::Failed
    );
    Ok(())
}

#[tokio::test]
async fn required_preparation_failure_blocks_every_stage_before_spawn() -> Result<()> {
    let board = BoardBackend::new();
    board.upsert(ParticipantStatus::new(
        "missing-driver",
        ParticipantKind::Driver,
        ParticipantState::Failed,
    ));
    board.upsert(ParticipantStatus::new(
        "later",
        ParticipantKind::Service,
        ParticipantState::Starting,
    ));
    let mut later = sleep_spec("later");
    later.bus_participant = false;

    let error = supervise_until_shutdown(
        vec![SupervisionStage::new(
            "later-stage",
            vec![later],
            WaitBudget::Bounded(Duration::from_secs(2)),
        )],
        board.clone(),
        SupervisorOptions::default(),
    )
    .await
    .expect_err("a required preparation failure must stop before spawning");

    assert!(error.to_string().contains("missing-driver"));
    assert_eq!(board.snapshot().participants["later"].pid, None);
    assert_eq!(
        board.supervisor_snapshot().lifecycle,
        phoxal_cli_core::session::ProjectLifecycle::Failed
    );
    Ok(())
}
