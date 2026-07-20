//! One bounded session log store shared by global and runtime-filtered views.
//! Structured bus severity is retained; raw child output is Info. Once a
//! runtime has emitted over the bus, later raw mirrors are dropped by routing
//! identity rather than fragile text comparison.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Instant, SystemTime};

use crate::session::event::DiagnosticLevel;
use crate::session::log::{LogScope, LogSeverity, LogSource, RoutedLogLine};

pub const RAW_LOG_CAPACITY: usize = 2000;
pub const BUS_LOG_CAPACITY: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayedLine {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
    pub received_at: Instant,
    pub event_time: SystemTime,
    order: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LogStore {
    /// Session-lifetime Raw/Bus cutover. Bus ingress is restricted to the
    /// finite launch plan before it reaches this store, so this set is bounded
    /// by the session graph and remains correct after both LRU and ring eviction.
    bus_participants: BTreeSet<String>,
    raw: VecDeque<DisplayedLine>,
    bus: BTreeMap<Option<LogScope>, VecDeque<DisplayedLine>>,
    next_order: u64,
}

impl LogStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, line: RoutedLogLine) {
        self.record_at(line, Instant::now());
    }

    pub fn replace_bus(&mut self, scope: LogScope, lines: Vec<RoutedLogLine>) {
        self.bus.remove(&Some(scope.clone()));
        for mut line in lines {
            line.scope = Some(scope.clone());
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
        let scope = line.scope.clone();
        let displayed = DisplayedLine {
            participant,
            source: line.source,
            severity: line.severity,
            text: sanitize_terminal_text(&line.text),
            received_at,
            event_time: line.event_time,
            order: self.next_order,
        };
        self.next_order = self.next_order.wrapping_add(1);
        match displayed.source {
            LogSource::Raw => insert_bounded(&mut self.raw, displayed, RAW_LOG_CAPACITY),
            LogSource::Bus => insert_bounded(
                self.bus.entry(scope).or_default(),
                displayed,
                BUS_LOG_CAPACITY,
            ),
        }
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
            event_time: SystemTime::now(),
            order: self.next_order,
        };
        self.next_order = self.next_order.wrapping_add(1);
        insert_bounded(&mut self.raw, displayed, RAW_LOG_CAPACITY);
    }

    #[must_use]
    pub fn lines(&self) -> Lines<'_> {
        Lines {
            store: self,
            after: None,
            before: None,
        }
    }
}

type LineKey = (SystemTime, u64);

fn line_key(line: &DisplayedLine) -> LineKey {
    (line.event_time, line.order)
}

fn insert_bounded(lines: &mut VecDeque<DisplayedLine>, line: DisplayedLine, capacity: usize) {
    let key = line_key(&line);
    let index = lines.partition_point(|existing| line_key(existing) <= key);
    lines.insert(index, line);
    if lines.len() > capacity {
        lines.pop_front();
    }
}

/// Allocation-free merge over the already sorted raw and robot-scoped bus
/// deques. Each step selects only the next head (or tail) from each source;
/// rendering never collects and re-sorts the complete retained history.
pub struct Lines<'a> {
    store: &'a LogStore,
    after: Option<LineKey>,
    before: Option<LineKey>,
}

impl<'a> Lines<'a> {
    fn next_in(&self, lines: &'a VecDeque<DisplayedLine>) -> Option<&'a DisplayedLine> {
        let index = self.after.map_or(0, |after| {
            lines.partition_point(|line| line_key(line) <= after)
        });
        let candidate = lines.get(index)?;
        self.before
            .is_none_or(|before| line_key(candidate) < before)
            .then_some(candidate)
    }

    fn next_back_in(&self, lines: &'a VecDeque<DisplayedLine>) -> Option<&'a DisplayedLine> {
        let end = self.before.map_or(lines.len(), |before| {
            lines.partition_point(|line| line_key(line) < before)
        });
        let candidate = end.checked_sub(1).and_then(|index| lines.get(index))?;
        self.after
            .is_none_or(|after| line_key(candidate) > after)
            .then_some(candidate)
    }
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a DisplayedLine;

    fn next(&mut self) -> Option<Self::Item> {
        let mut next = self.next_in(&self.store.raw);
        for lines in self.store.bus.values() {
            if let Some(candidate) = self.next_in(lines)
                && next.is_none_or(|current| line_key(candidate) < line_key(current))
            {
                next = Some(candidate);
            }
        }
        if let Some(line) = next {
            self.after = Some(line_key(line));
        }
        next
    }
}

impl DoubleEndedIterator for Lines<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let mut next = self.next_back_in(&self.store.raw);
        for lines in self.store.bus.values() {
            if let Some(candidate) = self.next_back_in(lines)
                && next.is_none_or(|current| line_key(candidate) > line_key(current))
            {
                next = Some(candidate);
            }
        }
        if let Some(line) = next {
            self.before = Some(line_key(line));
        }
        next
    }
}

impl std::iter::FusedIterator for Lines<'_> {}

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
    use std::time::{Duration, UNIX_EPOCH};

    fn line(id: &str, source: LogSource, text: &str) -> RoutedLogLine {
        RoutedLogLine {
            participant: id.to_string(),
            source,
            severity: LogSeverity::Info,
            text: text.to_string(),
            event_time: UNIX_EPOCH,
            scope: (source == LogSource::Bus).then(|| LogScope {
                namespace: "acme".to_string(),
                robot_id: "r1".to_string(),
            }),
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

        store.replace_bus(
            LogScope {
                namespace: "acme".to_string(),
                robot_id: "r1".to_string(),
            },
            vec![line("drive", LogSource::Bus, "snapshot")],
        );

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
        for index in 0..(BUS_LOG_CAPACITY + 5) {
            store.record(line("drive", LogSource::Bus, &format!("line {index}")));
        }
        assert_eq!(store.lines().count(), BUS_LOG_CAPACITY);
    }

    #[test]
    fn bus_replacement_keeps_raw_capacity_and_merges_by_event_time() {
        let mut store = LogStore::new();
        for index in 0..RAW_LOG_CAPACITY {
            let mut raw = line("drive", LogSource::Raw, &format!("raw {index}"));
            raw.event_time = UNIX_EPOCH + Duration::from_secs((index * 2 + 1) as u64);
            store.record(raw);
        }
        let mut bus = line("mission", LogSource::Bus, "retained");
        bus.event_time = UNIX_EPOCH + Duration::from_secs(2);
        store.replace_bus(
            LogScope {
                namespace: "acme".to_string(),
                robot_id: "r1".to_string(),
            },
            vec![bus],
        );

        let lines = store.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), RAW_LOG_CAPACITY + 1);
        assert_eq!(lines[0].text, "raw 0");
        assert_eq!(lines[1].text, "retained");
        assert_eq!(lines[2].text, "raw 1");
    }

    #[test]
    fn lines_k_way_merge_sorted_deques_from_both_ends() {
        let mut store = LogStore::new();
        let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
        for (seconds, text) in [(7, "raw 7"), (3, "raw 3")] {
            let mut raw = line("local", LogSource::Raw, text);
            raw.event_time = at(seconds);
            store.record(raw);
        }
        let scoped = |robot_id: &str| LogScope {
            namespace: "acme".to_string(),
            robot_id: robot_id.to_string(),
        };
        let retained = |seconds, text| {
            let mut bus = line("drive", LogSource::Bus, text);
            bus.event_time = at(seconds);
            bus
        };
        store.replace_bus(scoped("r1"), vec![retained(6, "r1 6"), retained(2, "r1 2")]);
        store.replace_bus(scoped("r2"), vec![retained(5, "r2 5"), retained(1, "r2 1")]);

        let mut lines = store.lines();
        assert_eq!(lines.next().unwrap().text, "r2 1");
        assert_eq!(lines.next_back().unwrap().text, "raw 7");
        assert_eq!(lines.next().unwrap().text, "r1 2");
        assert_eq!(lines.next_back().unwrap().text, "r1 6");
        assert_eq!(
            lines.map(|line| line.text.as_str()).collect::<Vec<_>>(),
            vec!["raw 3", "r2 5"]
        );
    }

    #[test]
    fn bus_replacement_is_scoped_per_robot() {
        let mut store = LogStore::new();
        let r1 = LogScope {
            namespace: "acme".to_string(),
            robot_id: "r1".to_string(),
        };
        let r2 = LogScope {
            namespace: "acme".to_string(),
            robot_id: "r2".to_string(),
        };
        store.replace_bus(r1.clone(), vec![line("drive-r1", LogSource::Bus, "one")]);
        store.replace_bus(r2, vec![line("drive-r2", LogSource::Bus, "two")]);
        store.replace_bus(r1, vec![line("drive-r1", LogSource::Bus, "new")]);

        let text = store
            .lines()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(text, vec!["two", "new"]);
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
    fn bus_cutover_survives_raw_ring_eviction_without_cross_source_eviction() {
        let mut store = LogStore::new();
        let started = Instant::now();
        store.record_at(line("drive", LogSource::Bus, "bus line"), started);
        for index in 0..RAW_LOG_CAPACITY {
            store.record_at(
                line("filler", LogSource::Raw, &format!("line {index}")),
                started + std::time::Duration::from_nanos(index as u64 + 1),
            );
        }
        assert!(
            store
                .lines()
                .any(|line| line.participant == "drive" && line.source == LogSource::Bus)
        );
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
