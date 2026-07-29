//! Bounded log-routing records shared by session adapters and presentation.

use std::time::SystemTime;

pub const MAX_ROUTED_LOG_TEXT_CHARS: usize = 4_096;

#[must_use]
pub fn bounded_log_text(text: &str) -> String {
    let mut chars = text.chars();
    let mut bounded = chars
        .by_ref()
        .take(MAX_ROUTED_LOG_TEXT_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

/// Where a routed log line came from; consumers deduplicate on this routing
/// identity rather than comparing rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    /// A structured `logs/{participant_id}` bus event
    /// A structured bus event: the primary source once a participant can
    /// publish on the bus.
    Bus,
    /// A captured stdout/stderr line from the supervised child process
    /// Captured child stdout/stderr: the source before bus connectivity.
    Raw,
}

/// Severity retained with routed logs so the global Logs page can filter
/// structured events without parsing their rendered text. Raw child output
/// has no typed level and is conservatively recorded as Info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogSeverity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogScope {
    pub namespace: String,
    pub robot_id: String,
}

/// One routed log line, separate from the persisted board's short history so
/// presentation can maintain its own bounded scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedLogLine {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
    /// Producer event time for retained records, receive time for local raw
    /// and diagnostic records. Presentation merges both bounded sources by
    /// this timestamp without rewriting retained history during a snapshot.
    pub event_time: SystemTime,
    /// Present for robot-owned retained records; local raw/diagnostic lines
    /// remain unscoped.
    pub scope: Option<LogScope>,
}

/// One update from the robot-owned retention tool to the presentation store.
/// A snapshot replaces only bus-derived lines; raw process and diagnostic
/// lines remain local presentation events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedLogUpdate {
    Replace {
        scope: LogScope,
        lines: Vec<RoutedLogLine>,
    },
    Append(RoutedLogLine),
}
