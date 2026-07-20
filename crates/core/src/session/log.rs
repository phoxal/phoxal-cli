//! Bounded log-routing records shared by session adapters and presentation.

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

/// One routed log line, separate from the persisted board's short history so
/// presentation can maintain its own bounded scrollback.
#[derive(Debug, Clone)]
pub struct RoutedLogLine {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
}
