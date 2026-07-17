//! One bounded session log store shared by global and runtime-filtered views.
//! Structured bus severity is retained; raw child output is Info. Once a
//! runtime has emitted over the bus, later raw mirrors are dropped by routing
//! identity rather than fragile text comparison.

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use crate::session::event::DiagnosticLevel;
use crate::supervisor::{LogSeverity, LogSource, RoutedLogLine};

pub const LOG_CAPACITY: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayedLine {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
    pub received_at: Instant,
}

#[derive(Debug, Clone, Default)]
struct RuntimeLogState {
    lines: VecDeque<DisplayedLine>,
    bus_seen: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LogStore {
    runtimes: BTreeMap<String, RuntimeLogState>,
    all: VecDeque<DisplayedLine>,
}

impl LogStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, line: RoutedLogLine) {
        self.record_at(line, Instant::now());
    }

    pub(crate) fn record_at(&mut self, line: RoutedLogLine, received_at: Instant) {
        let state = self.runtimes.entry(line.participant.clone()).or_default();
        if line.source == LogSource::Raw && state.bus_seen {
            return;
        }
        if line.source == LogSource::Bus {
            state.bus_seen = true;
        }
        let displayed = DisplayedLine {
            participant: line.participant,
            source: line.source,
            severity: line.severity,
            text: line.text,
            received_at,
        };
        push_bounded(&mut state.lines, displayed.clone());
        push_bounded(&mut self.all, displayed);
    }

    pub fn record_diagnostic(
        &mut self,
        participant: impl Into<String>,
        level: DiagnosticLevel,
        text: impl Into<String>,
    ) {
        let severity = match level {
            DiagnosticLevel::Info => LogSeverity::Info,
            DiagnosticLevel::Warn => LogSeverity::Warn,
            DiagnosticLevel::Error => LogSeverity::Error,
        };
        let participant = participant.into();
        let displayed = DisplayedLine {
            participant: participant.clone(),
            source: LogSource::Raw,
            severity,
            text: text.into(),
            received_at: Instant::now(),
        };
        push_bounded(
            &mut self.runtimes.entry(participant).or_default().lines,
            displayed.clone(),
        );
        push_bounded(&mut self.all, displayed);
    }

    #[must_use]
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &DisplayedLine> {
        self.all.iter()
    }

    #[must_use]
    #[cfg(test)]
    pub fn lines_for(&self, id: &str) -> impl DoubleEndedIterator<Item = &DisplayedLine> {
        self.runtimes
            .get(id)
            .into_iter()
            .flat_map(|state| state.lines.iter())
    }
}

fn push_bounded(lines: &mut VecDeque<DisplayedLine>, line: DisplayedLine) {
    lines.push_back(line);
    if lines.len() > LOG_CAPACITY {
        lines.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, source: LogSource, text: &str) -> RoutedLogLine {
        RoutedLogLine {
            participant: id.to_string(),
            source,
            severity: LogSeverity::Info,
            text: text.to_string(),
        }
    }

    #[test]
    fn bus_cutover_drops_only_later_raw_mirrors() {
        let mut store = LogStore::new();
        store.record(line("drive", LogSource::Raw, "booting"));
        store.record(line("drive", LogSource::Bus, "ready"));
        store.record(line("drive", LogSource::Raw, "ready"));
        let lines = store.lines_for("drive").collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "booting");
        assert_eq!(lines[1].source, LogSource::Bus);
    }

    #[test]
    fn session_and_runtime_views_share_the_same_entries() {
        let mut store = LogStore::new();
        store.record(line("drive", LogSource::Bus, "one"));
        store.record(line("mission", LogSource::Bus, "two"));
        assert_eq!(store.lines().count(), 2);
        assert_eq!(store.lines_for("drive").count(), 1);
    }

    #[test]
    fn global_and_runtime_rings_are_bounded() {
        let mut store = LogStore::new();
        for index in 0..(LOG_CAPACITY + 5) {
            store.record(line("drive", LogSource::Bus, &format!("line {index}")));
        }
        assert_eq!(store.lines().count(), LOG_CAPACITY);
        assert_eq!(store.lines_for("drive").count(), LOG_CAPACITY);
    }
}
