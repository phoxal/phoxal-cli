//! Non-authoritative view of the latest client-fenced log window.
//!
//! This type has no retention policy or ring. The attachment client supplies
//! an already bounded window and replacement discards the previous view.

use std::time::Instant;

use phoxal_cli_core::session::event::DiagnosticLevel;
pub use phoxal_cli_core::session::sanitize_terminal_text;
use phoxal_cli_core::session::{LogSeverity, LogSource, RoutedLogLine};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayedLine {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
    pub received_at: Instant,
    pub event_time: std::time::SystemTime,
    pub scope: Option<phoxal_cli_core::session::LogScope>,
}

#[derive(Debug, Clone, Default)]
pub struct LogView {
    rows: Vec<DisplayedLine>,
}

impl LogView {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace_all(&mut self, lines: Vec<RoutedLogLine>) {
        self.rows = lines.into_iter().map(displayed_line).collect();
        self.rows.sort_by_key(|line| line.event_time);
    }

    pub fn record_diagnostic(
        &mut self,
        participant: impl Into<String>,
        level: DiagnosticLevel,
        text: impl Into<String>,
    ) {
        self.rows.push(DisplayedLine {
            participant: sanitize_terminal_text(&participant.into()),
            source: LogSource::Raw,
            severity: match level {
                DiagnosticLevel::Info => LogSeverity::Info,
                DiagnosticLevel::Warn => LogSeverity::Warn,
                DiagnosticLevel::Error => LogSeverity::Error,
            },
            text: sanitize_terminal_text(&text.into()),
            received_at: Instant::now(),
            event_time: std::time::SystemTime::now(),
            scope: None,
        });
    }

    #[cfg(test)]
    pub fn record(&mut self, line: RoutedLogLine) {
        self.rows.push(displayed_line(line));
        self.rows.sort_by_key(|line| line.event_time);
    }

    #[cfg(test)]
    pub fn record_at(&mut self, line: RoutedLogLine, received_at: Instant) {
        let mut displayed = displayed_line(line);
        displayed.received_at = received_at;
        self.rows.push(displayed);
        self.rows.sort_by_key(|line| line.event_time);
    }

    #[must_use]
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &DisplayedLine> {
        self.rows.iter()
    }
}

fn displayed_line(line: RoutedLogLine) -> DisplayedLine {
    DisplayedLine {
        participant: sanitize_terminal_text(&line.participant),
        source: line.source,
        severity: line.severity,
        text: sanitize_terminal_text(&line.text),
        received_at: Instant::now(),
        event_time: line.event_time,
        scope: line.scope,
    }
}
