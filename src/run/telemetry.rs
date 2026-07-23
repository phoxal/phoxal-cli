//! Telemetry responsibilities for run.

use phoxal_cli_core::project::launch_plan::{LaunchPlan, ParticipantExecution};
use phoxal_cli_core::session::stores::telemetry::RobotScope;
use phoxal_cli_core::session::{ProcessScope, SupervisorSnapshotV0};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RobotFeedTarget {
    pub(crate) scope: RobotScope,
    pub(crate) participant_ids: Vec<String>,
}

impl RobotFeedTarget {
    pub(crate) fn from_plan(plan: &LaunchPlan) -> Vec<Self> {
        plan.robots
            .iter()
            .map(|robot| {
                let mut participant_ids = robot
                    .participants
                    .iter()
                    .filter(|participant| runtime_telemetry_candidate(&participant.execution))
                    .map(|participant| participant.launch.participant_id.clone())
                    .collect::<Vec<_>>();
                participant_ids.sort();
                participant_ids.dedup();
                Self {
                    scope: RobotScope {
                        namespace: robot.namespace.clone(),
                        robot_id: robot.id.clone(),
                    },
                    participant_ids,
                }
            })
            .collect()
    }

    pub(crate) fn from_snapshot(snapshot: &SupervisorSnapshotV0) -> Vec<Self> {
        let mut targets = std::collections::BTreeMap::<(String, String), Vec<String>>::new();
        for key in snapshot.processes.keys() {
            if let ProcessScope::Robot(robot) = &key.scope {
                targets
                    .entry((robot.namespace.clone(), robot.robot_id.clone()))
                    .or_default()
                    .push(key.id.clone());
            }
        }
        targets
            .into_iter()
            .map(|((namespace, robot_id), mut participant_ids)| {
                participant_ids.sort();
                participant_ids.dedup();
                Self {
                    scope: RobotScope {
                        namespace,
                        robot_id,
                    },
                    participant_ids,
                }
            })
            .collect()
    }
}

fn runtime_telemetry_candidate(execution: &ParticipantExecution) -> bool {
    match execution {
        ParticipantExecution::OfficialTool { .. } => false,
        ParticipantExecution::SourceArtifact { kind, .. } if kind == "tool" => false,
        _ => true,
    }
}

/// Start the optional session-scoped telemetry feeds for a running graph.
pub(crate) fn start_telemetry_feeds_at(
    robot_targets: &[RobotFeedTarget],
    telemetry: &crate::telemetry::TelemetryBackend,
    connect: &str,
    recovery_epochs: tokio::sync::watch::Receiver<u64>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some(first) = robot_targets.first() else {
        return Vec::new();
    };
    let namespace = &first.scope.namespace;
    let robot_id = &first.scope.robot_id;
    let mut feeds = vec![
        crate::telemetry::start_joypad_devices_feed(
            namespace.clone(),
            robot_id.clone(),
            connect.to_string(),
            telemetry.clone(),
        ),
        crate::telemetry::start_control_state_feed(
            namespace.clone(),
            robot_id.clone(),
            connect.to_string(),
            telemetry.clone(),
        ),
    ];
    feeds.extend(robot_targets.iter().flat_map(|target| {
        [
            crate::telemetry::start_device_feed(
                target.scope.namespace.clone(),
                target.scope.robot_id.clone(),
                connect.to_string(),
                telemetry.clone(),
                recovery_epochs.clone(),
            ),
            crate::telemetry::start_router_metrics_feed(
                target.scope.namespace.clone(),
                target.scope.robot_id.clone(),
                connect.to_string(),
                telemetry.clone(),
                recovery_epochs.clone(),
            ),
            crate::telemetry::start_runtime_performance_feed(
                target.scope.namespace.clone(),
                target.scope.robot_id.clone(),
                target.participant_ids.clone(),
                connect.to_string(),
                telemetry.clone(),
                recovery_epochs.clone(),
            ),
        ]
    }));
    feeds
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn runtime_snapshot_fanout_excludes_official_and_overridden_tools() {
        assert!(!runtime_telemetry_candidate(
            &ParticipantExecution::OfficialTool {
                artifact_ref: "tool-telemetry@0.1.5".to_string(),
            }
        ));
        assert!(!runtime_telemetry_candidate(
            &ParticipantExecution::SourceArtifact {
                kind: "tool".to_string(),
                crate_dir: PathBuf::from("tool/telemetry"),
            }
        ));
        assert!(runtime_telemetry_candidate(
            &ParticipantExecution::SourceArtifact {
                kind: "service".to_string(),
                crate_dir: PathBuf::from("service/map"),
            }
        ));
    }
}
