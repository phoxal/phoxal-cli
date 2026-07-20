//! Telemetry responsibilities for run.

/// Start the optional session-scoped telemetry feeds for a running graph.
pub(crate) fn start_telemetry_feeds_at(
    robot_log_targets: &[(String, String)],
    telemetry: &crate::telemetry::TelemetryBackend,
    connect: &str,
    recovery_epochs: tokio::sync::watch::Receiver<u64>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some((namespace, robot_id)) = robot_log_targets.first() else {
        return Vec::new();
    };
    let mut feeds = vec![
        crate::telemetry::start_host_feed(
            namespace.clone(),
            robot_id.clone(),
            connect.to_string(),
            telemetry.clone(),
        ),
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
    feeds.extend(robot_log_targets.iter().flat_map(|(namespace, robot_id)| {
        [
            crate::telemetry::start_router_metrics_feed(
                namespace.clone(),
                robot_id.clone(),
                connect.to_string(),
                telemetry.clone(),
                recovery_epochs.clone(),
            ),
            crate::telemetry::start_runtime_performance_feed(
                namespace.clone(),
                robot_id.clone(),
                connect.to_string(),
                telemetry.clone(),
                recovery_epochs.clone(),
            ),
        ]
    }));
    feeds
}
