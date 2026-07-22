use std::time::Duration;

pub const RESTART_SEC: Duration = Duration::from_secs(2);
pub const START_LIMIT_INTERVAL: Duration = Duration::from_secs(60);
pub const START_LIMIT_BURST: usize = 5;

const MAX_LOG_TEXT_CHARS: usize = 4_096;
/// Four bytes per Unicode scalar keeps a complete display line bounded before
/// lossy UTF-8 decoding. Bytes beyond this limit are drained and represented
/// by one ellipsis when the newline (or EOF) arrives.
const MAX_CAPTURED_LINE_BYTES: usize = MAX_LOG_TEXT_CHARS * 4;

fn bounded_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn bounded_log_text(text: &str) -> String {
    bounded_chars(text, MAX_LOG_TEXT_CHARS)
}

pub use phoxal_cli_core::session::board::{
    BoardSnapshot, ParticipantLaunchCommand, ParticipantState, ParticipantStatus,
};
pub use phoxal_cli_core::session::log::{
    LogScope, LogSeverity, LogSource, RoutedLogLine, RoutedLogUpdate,
};

mod board;
pub(crate) use board::BoardBackend;
mod spec;
pub(crate) use spec::{
    ParticipantSpec, RequestedStop, RestartPolicy, SupervisorAction, SupervisorActionReceiver,
    SupervisorOptions, SupervisorOutcome,
};
mod process;
pub(crate) use process::RunningParticipant;
mod output;
pub(crate) use output::{
    READER_JOIN_BUDGET, join_reader, requested_stop_exit_is_clean, spawn_output_reader,
};
mod stages;
#[cfg(test)]
pub(crate) use stages::await_participants_ready;
pub(crate) use stages::{
    SupervisionStage, await_stage_ready, emit_event, maybe_emit_staged_startup_complete,
    orderly_shutdown_budget, spawn_until_pending,
};
mod r#loop;
pub(crate) use r#loop::supervise_until_shutdown;
mod signals;
pub(crate) use signals::{
    ensure_process_group_stopped, force_kill_supervised_process_groups, kill_child_process_group,
    send_process_group_signal, send_process_group_terminate, send_process_signal, send_terminate,
    stop_child,
};
mod lock;
pub(crate) use lock::{SupervisorIdentity, SupervisorLock};
mod bus;
pub(crate) use bus::{
    default_connect_endpoint, failed_expected_participants, missing_ready_participants,
    start_bus_log_subscriber, start_clock_feed, start_liveliness_observer,
    start_robot_description_server, wait_for_endpoint,
};

#[cfg(test)]
mod tests;
