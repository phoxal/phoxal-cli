//! Tests for this module.

use super::build::missing_device_path;
use super::report::{
    LaunchCommandEntry, LaunchCommandReport, launch_kind_label, report_launch_commands_human,
};
use super::router::parse_router_ready;
use super::*;
use anyhow::Result;
use phoxal::model::robot::v0::ConnectionConfig;
use phoxal::participant::launch::env;
use phoxal::participant::launch::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
};
use phoxal_cli_core::project::launch_plan::{
    LaunchMode, LaunchOwnership, LaunchPlan, ParticipantExecution, ParticipantLaunchRecord,
};
use phoxal_cli_core::session::{ParticipantKind, RuntimeFailurePolicy, StartupRequirement};
use std::path::PathBuf;

use crate::supervisor::{ParticipantSpec, default_connect_endpoint};

#[test]
fn human_launch_report_enters_the_active_session_diagnostics() -> Result<()> {
    let _guard = crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK.blocking_lock();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    crate::session::diagnostics::install(tx);
    let output = LaunchCommandReport {
        participants: vec![LaunchCommandEntry {
            id: "drive".to_string(),
            kind: "official",
            command_line: "service-drive".to_string(),
        }],
    };

    let result = report_launch_commands_human(&output, &crate::Ui::new(true));
    crate::session::diagnostics::uninstall();
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
    assert!(messages[1].contains("drive (official) -> service-drive"));
    assert!(messages[2].starts_with("motion guarantees:"));
    Ok(())
}

fn participant(id: &str, execution: ParticipantExecution) -> ParticipantLaunchRecord {
    ParticipantLaunchRecord {
        artifact_id: id.to_string(),
        execution,
        launch: ParticipantLaunch {
            participant_id: id.to_string(),
            incarnation: 0,
            namespace: "dev".to_string(),
            robot_id: "robot".to_string(),
            bus: BusProfile {
                connect_endpoints: vec![default_connect_endpoint()],
            },
            clock: ClockMode::Real,
            config: None,
            robot_root: Some(PathBuf::from("/tmp/robot")),
            component_instance: None,
            execution_device_id: None,
            shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
        },
        launch_ownership: LaunchOwnership::CliManaged,
        startup_requirement: StartupRequirement::Required,
        runtime_failure: RuntimeFailurePolicy::StopProject,
    }
}

fn plan_with_drivers(ids: &[&str]) -> LaunchPlan {
    LaunchPlan {
        mode: LaunchMode::Run,
        robots: vec![phoxal_cli_core::project::launch_plan::RobotLaunch {
            id: "robot".to_string(),
            namespace: "dev".to_string(),
            participants: ids
                .iter()
                .map(|id| {
                    participant(
                        id,
                        ParticipantExecution::ComponentDriver {
                            crate_dir: PathBuf::from("/tmp/driver"),
                        },
                    )
                })
                .collect(),
            substitutions: Vec::new(),
        }],
    }
}

#[test]
fn driver_subset_is_strict() -> Result<()> {
    let plan = plan_with_drivers(&["imu", "left_drive"]);
    let policy = DriverPolicy::from_options(
        &RunOptions {
            drivers: DriversMode::On,
            drivers_subset: vec!["imu".to_string()],
            suite_source: None,
            watch: false,
        },
        &plan,
    )?;
    assert_eq!(policy.decision("imu"), DriverDecision::Launch);
    assert_eq!(
        policy.decision("left_drive"),
        DriverDecision::Degraded("not selected by --driver".to_string())
    );

    let err = DriverPolicy::from_options(
        &RunOptions {
            drivers: DriversMode::On,
            drivers_subset: vec!["missing".to_string()],
            suite_source: None,
            watch: false,
        },
        &plan,
    )
    .expect_err("unknown drivers must fail");
    assert!(err.to_string().contains("unknown driver id"));
    Ok(())
}

#[test]
fn drivers_off_degrades_every_driver() -> Result<()> {
    let plan = plan_with_drivers(&["imu"]);
    let policy = DriverPolicy::from_options(
        &RunOptions {
            drivers: DriversMode::Off,
            drivers_subset: Vec::new(),
            suite_source: None,
            watch: false,
        },
        &plan,
    )?;
    assert_eq!(
        policy.decision("imu"),
        DriverDecision::Degraded("drivers off".to_string())
    );
    Ok(())
}

#[test]
fn serial_device_missing_is_loud() {
    let missing = missing_device_path(&ConnectionConfig::Serial {
        port: "/definitely/not/a/phoxal/device".to_string(),
        baud: 115200,
    });
    assert_eq!(missing.as_deref(), Some("/definitely/not/a/phoxal/device"));
}

/// The launch report's `kind` string stays byte-identical even though the
/// board's own `ParticipantKind` is now the finer-grained shared
/// `Tool`/`Service`/`Driver`/`Simulator` split plus a `local` bit (Part
/// 1 kind consolidation) - see `launch_kind_label`'s docs.
#[test]
fn launch_kind_label_matches_the_operator_facing_strings() {
    assert_eq!(launch_kind_label(None), "robot-tool");
    assert_eq!(
        launch_kind_label(Some(&ParticipantExecution::OfficialArtifact {
            artifact_ref: "phoxal/service-drive@1.0.0".to_string(),
        })),
        "official"
    );
    assert_eq!(
        launch_kind_label(Some(&ParticipantExecution::SourceArtifact {
            kind: "service".to_string(),
            crate_dir: PathBuf::from("/tmp/drive"),
        })),
        "official",
        "a locally source-overridden official artifact stayed bucketed as \"official\" pre-consolidation"
    );
    assert_eq!(
        launch_kind_label(Some(&ParticipantExecution::UserService {
            crate_dir: PathBuf::from("/tmp/mission"),
        })),
        "user-service"
    );
    assert_eq!(
        launch_kind_label(Some(&ParticipantExecution::ComponentDriver {
            crate_dir: PathBuf::from("/tmp/ddsm115"),
        })),
        "driver"
    );
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

#[test]
fn router_ready_fd_contract_returns_the_exact_local_endpoint() {
    assert_eq!(
        parse_router_ready(r#"{"status":"ready","local_endpoint":"unixsock-stream//tmp/zenoh.sock","listeners":["unixsock-stream//tmp/zenoh.sock"],"future":true}"#)
            .expect("ready event"),
        "unixsock-stream//tmp/zenoh.sock"
    );
    assert!(parse_router_ready(r#"{"status":"failed","message":"bind refused"}"#).is_err());
    assert!(parse_router_ready(r#"{"status":"starting"}"#).is_err());
}

#[test]
fn selected_router_endpoint_reaches_plan_and_spawn_environment() {
    let mut plan = plan_with_drivers(&["imu"]);
    let mut specs = vec![ParticipantSpec {
        key: phoxal_cli_core::session::ProcessKey::project("tool-bus"),
        id: "tool-bus".to_string(),
        kind: ParticipantKind::Tool,
        executable: PathBuf::from("/tmp/tool-bus"),
        args: Vec::new(),
        cwd: None,
        env: vec![(
            env::CONNECT.to_string(),
            phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT.to_string(),
        )],
        shutdown_grace: Duration::from_secs(1),
        process_group: true,
        note: None,
        bus_participant: true,
        readiness: ParticipantSpec::exact_liveliness_template(
            phoxal_cli_core::session::RobotKey::new("test", "robot"),
            "tool-bus",
        ),
        startup_requirement: phoxal_cli_core::session::StartupRequirement::Optional,
        runtime_failure: phoxal_cli_core::session::RuntimeFailurePolicy::KeepProjectDegraded,
        restart_policy: Default::default(),
    }];

    apply_session_connect(&mut plan, &mut specs, "tcp/127.0.0.1:7448");

    assert!(
        plan.robots
            .iter()
            .flat_map(|robot| &robot.participants)
            .all(|participant| participant.launch.bus.connect_endpoints == ["tcp/127.0.0.1:7448"])
    );
    assert_eq!(
        specs[0]
            .env
            .iter()
            .find(|(key, _)| key == env::CONNECT)
            .map(|(_, value)| value.as_str()),
        Some("tcp/127.0.0.1:7448")
    );
}
