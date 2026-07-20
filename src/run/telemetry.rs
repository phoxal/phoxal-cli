//! Telemetry responsibilities for run.

use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::session::stores::telemetry::RobotScope;

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
