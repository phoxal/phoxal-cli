//! One bounded session log store shared by global and runtime-filtered views.
//! Structured bus severity is retained; raw child output is Info. Once a
//! runtime has emitted over the bus, later raw mirrors are dropped by routing
//! identity rather than fragile text comparison.

use std::collections::{BTreeSet, VecDeque};
use std::time::Instant;

use crate::session::event::DiagnosticLevel;
use crate::session::log::{LogSeverity, LogSource, RoutedLogLine};

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
pub struct LogStore {
    /// Session-lifetime Raw/Bus cutover. Bus ingress is restricted to the
    /// finite launch plan before it reaches this store, so this set is bounded
    /// by the session graph and remains correct after both LRU and ring eviction.
    bus_participants: BTreeSet<String>,
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

    pub fn replace_bus(&mut self, lines: Vec<RoutedLogLine>) {
        self.all.retain(|line| line.source != LogSource::Bus);
        for line in lines {
            self.record(line);
        }
    }

    #[doc(hidden)]
    pub fn record_at(&mut self, line: RoutedLogLine, received_at: Instant) {
        let participant = sanitize_terminal_text(&line.participant);
        if line.source == LogSource::Bus {
            self.bus_participants.insert(participant.clone());
        }
        if line.source == LogSource::Raw && self.bus_participants.contains(&participant) {
            return;
        }
        let displayed = DisplayedLine {
            participant,
            source: line.source,
            severity: line.severity,
            text: sanitize_terminal_text(&line.text),
            received_at,
        };
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
        let participant = sanitize_terminal_text(&participant.into());
        let text = text.into();
        let received_at = Instant::now();
        let displayed = DisplayedLine {
            participant: participant.clone(),
            source: LogSource::Raw,
            severity,
            text: sanitize_terminal_text(&text),
            received_at,
        };
        push_bounded(&mut self.all, displayed);
    }

    #[must_use]
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &DisplayedLine> {
        self.all.iter()
    }
}

fn push_bounded(lines: &mut VecDeque<DisplayedLine>, line: DisplayedLine) {
    lines.push_back(line);
    if lines.len() > LOG_CAPACITY {
        lines.pop_front();
    }
}

/// Ratatui ultimately writes cell symbols to the real terminal. Keeping an
/// embedded CSI/OSC sequence in a log line therefore lets participant output
/// move the terminal cursor during a frame, which used to leave the scattered
/// `h`/`n`/`z` markers visible after leaving Logs. Strip ANSI first, then
/// neutralize every remaining control character before text reaches a cell.
pub fn sanitize_terminal_text(text: &str) -> String {
    strip_ansi(text)
        .chars()
        .map(|character| {
            if character.is_control() || is_terminal_format_control(character) {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    output
}

fn is_terminal_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{13455}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
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
        let lines = store.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "booting");
        assert_eq!(lines[1].source, LogSource::Bus);
    }

    #[test]
    fn one_global_ring_contains_every_participant() {
        let mut store = LogStore::new();
        store.record(line("drive", LogSource::Bus, "one"));
        store.record(line("mission", LogSource::Bus, "two"));
        assert_eq!(store.lines().count(), 2);
        assert_eq!(
            store
                .lines()
                .filter(|line| line.participant == "drive")
                .count(),
            1
        );
    }

    #[test]
    fn snapshot_replaces_only_bus_owned_lines() {
        let mut store = LogStore::new();
        store.record(line("drive", LogSource::Raw, "booting"));
        store.record(line("drive", LogSource::Bus, "old retained"));

        store.replace_bus(vec![line("drive", LogSource::Bus, "snapshot")]);

        let lines = store.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "booting");
        assert_eq!(lines[0].source, LogSource::Raw);
        assert_eq!(lines[1].text, "snapshot");
        assert_eq!(lines[1].source, LogSource::Bus);
    }

    #[test]
    fn global_ring_is_bounded() {
        let mut store = LogStore::new();
        for index in 0..(LOG_CAPACITY + 5) {
            store.record(line("drive", LogSource::Bus, &format!("line {index}")));
        }
        assert_eq!(store.lines().count(), LOG_CAPACITY);
    }

    #[test]
    fn unrelated_participants_do_not_reset_bus_cutover() {
        let mut store = LogStore::new();
        let started = Instant::now();
        store.record_at(line("drive", LogSource::Bus, "bus line"), started);
        for index in 0..256 {
            store.record_at(
                line(&format!("participant-{index}"), LogSource::Raw, "raw"),
                started + std::time::Duration::from_nanos(index as u64 + 1),
            );
        }
        let before = store.lines().count();
        store.record_at(
            line("drive", LogSource::Raw, "mirrored bus line"),
            started + std::time::Duration::from_secs(1),
        );
        assert_eq!(store.lines().count(), before);
    }

    #[test]
    fn bus_cutover_survives_lru_and_global_ring_eviction() {
        let mut store = LogStore::new();
        let started = Instant::now();
        store.record_at(line("drive", LogSource::Bus, "bus line"), started);
        for index in 0..LOG_CAPACITY {
            store.record_at(
                line("filler", LogSource::Raw, &format!("line {index}")),
                started + std::time::Duration::from_nanos(index as u64 + 1),
            );
        }
        assert!(store.lines().all(|line| line.participant != "drive"));
        store.record_at(
            line("drive", LogSource::Raw, "mirrored bus line"),
            started + std::time::Duration::from_secs(1_000),
        );
        assert!(
            store
                .lines()
                .all(|line| line.participant != "drive" || line.source == LogSource::Bus)
        );
    }

    #[test]
    fn terminal_control_sequences_are_never_retained() {
        let mut store = LogStore::new();
        store.record(line(
            "drive\u{1b}[2J\u{202e}",
            LogSource::Raw,
            "before\u{1b}[7;39Hafter\r\nnext\u{1b}]8;;https://example.com\u{7}link\u{9b}2J\u{7f}\u{202e}",
        ));
        let line = store.lines().next().unwrap();
        assert_eq!(line.participant, "drive ");
        let text = &line.text;
        assert!(text.contains("beforeafter  next"));
        assert!(!text.chars().any(char::is_control));
        assert!(!text.chars().any(is_terminal_format_control));
    }

    #[test]
    fn diagnostics_sanitize_participant_keys_and_text() {
        let mut store = LogStore::new();
        store.record_diagnostic(
            "dr\u{202e}ive",
            DiagnosticLevel::Error,
            "failed\u{202e}spoof",
        );
        let line = store.lines().next().unwrap();
        assert_eq!(line.participant, "dr ive");
        assert_eq!(line.text, "failed spoof");
    }
}
