//! The in-memory launch plan.
//!
//! The plan is never persisted. It is derived from the finalized bundle at
//! startup - the manifest, the CLI-internal catalog, and the embedded metadata
//! of the binaries under `bin/` - and it dies with the daemon. A restart
//! derives it again from the same two authorities, which is why there is
//! nothing on disk for the two to disagree about.
//!
//! Producers are not planned. Each participant's producer is the ZID of the
//! Zenoh session it opens, so nothing here can name one in advance.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use phoxal_cli_core::project::launch_plan::{LaunchPlan, ParticipantExecution};
use phoxal_cli_core::runtime::{
    ParticipantKind, ParticipantSpec, ProcessKey, RobotKey, RuntimeFailurePolicy,
    encode_participant_env,
};

/// Turn the constructed launch plan into the specs the process machinery
/// spawns, with every participant dialing this daemon's own router.
///
/// The endpoint is applied here rather than being carried through plan
/// construction: the router does not exist until the execution identity has
/// been minted and the router opened, and a plan that named an endpoint before
/// that could only ever name a guess.
pub(crate) fn participant_specs(
    plan: &LaunchPlan,
    bin_dir: &Path,
    connect_endpoint: &str,
) -> Result<Vec<ParticipantSpec>> {
    let mut specs = Vec::new();
    for robot in &plan.robots {
        let robot_key = RobotKey::new(robot.namespace.clone(), robot.id.clone());
        for participant in &robot.participants {
            let mut launch = participant.launch.clone();
            launch.bus.connect_endpoints = vec![connect_endpoint.to_string()];
            let id = launch.participant_id.clone();
            let env = encode_participant_env(&launch).with_context(|| {
                format!("failed to encode the launch environment for participant `{id}`")
            })?;
            specs.push(ParticipantSpec {
                key: ProcessKey::robot(robot_key.clone(), &id),
                kind: kind(&participant.execution),
                // A bundle has no source tree, so a participant runs from
                // nowhere in particular: every path it needs is resolved
                // through the bundle root its launch record carries.
                cwd: None,
                executable: bin_dir.join(binary_name(&participant.execution)),
                args: Vec::new(),
                env: env.spawn_env(),
                shutdown_grace: Duration::from_millis(launch.shutdown_grace_ms),
                // Its own group, so a graceful stop reaches whatever the
                // participant itself spawned.
                process_group: true,
                note: None,
                bus_participant: true,
                readiness: ParticipantSpec::exact_liveliness(robot_key.clone(), &id),
                startup_requirement: participant.startup_requirement,
                runtime_failure: RuntimeFailurePolicy::StopProject,
                restart_policy: phoxal_cli_core::runtime::RestartPolicy::default(),
                id,
            });
        }
    }
    Ok(specs)
}

const fn kind(execution: &ParticipantExecution) -> ParticipantKind {
    match execution {
        ParticipantExecution::Brain { .. } => ParticipantKind::Brain,
        ParticipantExecution::OfficialArtifact { .. }
        | ParticipantExecution::UserService { .. } => ParticipantKind::Service,
        ParticipantExecution::ComponentDriver { .. } => ParticipantKind::Driver,
    }
}

fn binary_name(execution: &ParticipantExecution) -> &str {
    match execution {
        ParticipantExecution::Brain { binary_name }
        | ParticipantExecution::OfficialArtifact { binary_name }
        | ParticipantExecution::UserService { binary_name }
        | ParticipantExecution::ComponentDriver { binary_name } => binary_name,
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::project::launch_plan::{LaunchMode, ParticipantLaunchRecord, RobotLaunch};
    use phoxal_cli_core::runtime::StartupRequirement;
    use phoxal_runtime_contract::{
        BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ExecutionOrigin, ParticipantLaunch, env,
    };

    use super::*;

    fn plan() -> LaunchPlan {
        let launch = |participant_id: &str| ParticipantLaunch {
            participant_id: participant_id.to_string(),
            execution: phoxal_cli_core::identity::ExecutionId::mint(),
            execution_origin: Some(ExecutionOrigin::mint()),
            robot_id: "rover".to_string(),
            bus: BusProfile {
                // Deliberately the placeholder plan construction writes: the
                // daemon must replace it with its own router.
                connect_endpoints: vec!["tcp/localhost:7447".to_string()],
            },
            clock: ClockMode::Simulation,
            config: None,
            bundle_root: Some(std::path::PathBuf::from("/bundle")),
            component_instance: None,
            shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
        };
        LaunchPlan {
            mode: LaunchMode::Run,
            robots: vec![RobotLaunch {
                id: "rover".to_string(),
                namespace: "demo".to_string(),
                participants: vec![
                    ParticipantLaunchRecord {
                        artifact_id: "brain".to_string(),
                        execution: ParticipantExecution::Brain {
                            binary_name: "brain".to_string(),
                        },
                        launch: launch("brain"),
                        startup_requirement: StartupRequirement::Required,
                        runtime_failure: RuntimeFailurePolicy::StopProject,
                    },
                    ParticipantLaunchRecord {
                        artifact_id: "ddsm115".to_string(),
                        execution: ParticipantExecution::ComponentDriver {
                            binary_name: "phoxal-driver-ddsm115".to_string(),
                        },
                        launch: launch("left"),
                        startup_requirement: StartupRequirement::Required,
                        runtime_failure: RuntimeFailurePolicy::StopProject,
                    },
                ],
            }],
        }
    }

    #[test]
    fn every_participant_dials_this_daemons_router_from_its_canonical_binary() {
        let specs = participant_specs(
            &plan(),
            Path::new("/bundle/bin"),
            "unixsock-stream//run/supervisor.sock",
        )
        .expect("the specs build");

        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].executable, Path::new("/bundle/bin/brain"));
        assert_eq!(specs[0].kind, ParticipantKind::Brain);
        assert_eq!(
            specs[1].executable,
            Path::new("/bundle/bin/phoxal-driver-ddsm115")
        );
        assert_eq!(specs[1].kind, ParticipantKind::Driver);

        for spec in &specs {
            let connect = spec
                .env
                .iter()
                .find(|(key, _)| key == env::CONNECT)
                .map(|(_, value)| value.as_str())
                .expect("every participant is told where to dial");
            assert_eq!(
                connect, "unixsock-stream//run/supervisor.sock",
                "the plan's placeholder endpoint must not survive"
            );
            // Nothing plans a producer: it is the ZID of the session the
            // participant itself opens.
            assert!(
                !spec.env.iter().any(|(key, _)| key.contains("PRODUCER")),
                "{:?}",
                spec.env
            );
            assert!(spec.cwd.is_none(), "a bundle run has no source directory");
            assert!(spec.process_group, "a participant owns its own group");
        }
    }

    #[test]
    fn readiness_is_the_participants_own_liveliness_token() {
        let specs = participant_specs(&plan(), Path::new("/bundle/bin"), "tcp/127.0.0.1:7447")
            .expect("the specs build");
        for spec in specs {
            assert_eq!(
                spec.readiness,
                ParticipantSpec::exact_liveliness(RobotKey::new("demo", "rover"), &spec.id),
                "a spawned process is not a ready one"
            );
        }
    }
}
