//! Concurrent session-board state derived from process and bus observations.

use super::{
    BoardSnapshot, LogSeverity, LogSource, ParticipantLaunchCommand, ParticipantState,
    ParticipantStatus, RoutedLogLine, bounded_chars, bounded_log_text,
};
use phoxal_cli_core::session::ParticipantKind;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;

const NOT_PRESENT_NOTE: &str =
    "participant not present on the robot bus; process lifecycle is unchanged";

#[derive(Debug, Clone, Default)]
pub struct BoardBackend {
    inner: Arc<Mutex<BoardSnapshot>>,
    /// Current robot-scoped Liveliness membership. This mirrors Zenoh's
    /// binary present/not-present observation without timestamps, leases, or
    /// inferred instances. Replacement processes may overlap under one stable
    /// key, producing continuous presence rather than another `Alive` event.
    present: Arc<Mutex<BTreeSet<String>>>,
    /// Optional live sink for [`RoutedLogLine`]s - set by
    /// `session::controller::SessionController::drive_supervision` once its
    /// `TuiDisplay` renderer exists, so it can maintain its own bounded
    /// per-runtime scrollback (Part 3) without polling the board's own
    /// 8-line history. `None` outside a TUI session (Plain/Json output, or
    /// any test) - [`Self::route_log`] simply skips the broadcast in that
    /// case. Bounded: the consuming `TuiDisplay::redraw` already keeps its
    /// own bounded ring per runtime (`stores::log_store::LogStore`), so a
    /// full channel here just means a redraw is overdue - `route_log` drops
    /// the newest line rather than blocking the log-subscriber task that
    /// calls it.
    log_sink: Arc<Mutex<Option<mpsc::Sender<RoutedLogLine>>>>,
    /// First-seen unknown bus ids already disclosed to the operator. The set
    /// is deliberately small: an untrusted publisher cannot turn diagnostics
    /// about rejected ids into another unbounded allocation vector.
    unknown_bus_ids: Arc<Mutex<BTreeSet<String>>>,
}

impl BoardBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record Zenoh Liveliness for a planned participant.
    ///
    /// Launch planning must register every expected row before the observer
    /// starts. Presence for an unknown id is dropped rather than deferred, so
    /// untrusted bus keys cannot grow supervisor state.
    ///
    /// Appearance is the sole transition to `Ready`. Disappearance is
    /// observational: it degrades a participant that was ready, but never
    /// marks it failed or commands a restart. Direct process lifecycle and
    /// startup timeouts retain that authority. Observations cannot resurrect
    /// terminal process state.
    pub fn record_presence(&self, id: &str, present: bool) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        let Some(status) = snapshot.participants.get_mut(id) else {
            drop(snapshot);
            self.disclose_unknown_bus_id(id, "liveliness");
            return;
        };
        if present {
            self.present
                .lock()
                .expect("presence mutex poisoned")
                .insert(id.to_string());
        } else {
            self.present
                .lock()
                .expect("presence mutex poisoned")
                .remove(id);
        }
        if matches!(
            status.state,
            ParticipantState::Failed | ParticipantState::Stopped
        ) {
            return;
        }
        if present {
            let clears_absence_note = status.state == ParticipantState::Degraded
                && status.note.as_deref() == Some(NOT_PRESENT_NOTE);
            status.state = ParticipantState::Ready;
            if clears_absence_note {
                status.note = None;
            }
        } else if status.state == ParticipantState::Ready {
            status.state = ParticipantState::Degraded;
            status.note = Some(NOT_PRESENT_NOTE.to_string());
        }
    }

    #[must_use]
    pub fn is_present(&self, id: &str) -> bool {
        self.present
            .lock()
            .expect("presence mutex poisoned")
            .contains(id)
    }

    pub fn upsert(&self, status: ParticipantStatus) {
        self.inner
            .lock()
            .expect("board mutex poisoned")
            .participants
            .insert(status.id.clone(), status);
    }

    pub(crate) fn register_planned(&self, id: &str, kind: ParticipantKind) {
        self.inner
            .lock()
            .expect("board mutex poisoned")
            .participants
            .entry(id.to_string())
            .or_insert_with(|| ParticipantStatus::new(id, kind, ParticipantState::Starting));
    }

    pub fn set_state(&self, id: &str, state: ParticipantState, note: Option<String>) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.state = state;
            if matches!(
                state,
                ParticipantState::Starting
                    | ParticipantState::Restarting
                    | ParticipantState::Stopped
            ) {
                status.pid = None;
                status.artifact_size_bytes = None;
            }
            if note.is_some() {
                status.note = note;
            }
        }
    }

    pub fn set_note(&self, id: &str, note: impl Into<String>) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.note = Some(note.into());
        }
    }

    pub fn set_restart_count(&self, id: &str, count: u32) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.restart_count = count;
        }
    }

    pub fn append_log(&self, id: &str, line: impl Into<String>) {
        let _ = self.try_append_log(id, line);
    }

    fn try_append_log(&self, id: &str, line: impl Into<String>) -> bool {
        let line = bounded_log_text(&line.into());
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        let Some(status) = snapshot.participants.get_mut(id) else {
            return false;
        };
        status.last_log_line = Some(line.clone());
        status.last_log_lines.push(line);
        const MAX_LAST_LINES: usize = 8;
        if status.last_log_lines.len() > MAX_LAST_LINES {
            let drop_count = status.last_log_lines.len() - MAX_LAST_LINES;
            status.last_log_lines.drain(0..drop_count);
        }
        true
    }

    /// Register the live [`RoutedLogLine`] sink for this session's display
    /// (a TUI). Replaces any previous sink - only one live display exists per
    /// session.
    pub fn set_log_sink(&self, sender: mpsc::Sender<RoutedLogLine>) {
        *self.log_sink.lock().expect("log sink mutex poisoned") = Some(sender);
    }

    /// Record a log line from a known routing source: updates the board's own
    /// bounded 8-line history exactly like [`Self::append_log`], then
    /// additionally forwards it to the live sink
    /// registered by [`Self::set_log_sink`], if any. This is the single
    /// funnel both [`bus_log_subscriber_loop`] and [`spawn_output_reader`]
    /// use, so a live TUI dedups by ROUTING (which of the two called this)
    /// rather than by comparing rendered text - see [`LogSource`].
    pub fn route_log(&self, id: &str, source: LogSource, text: impl Into<String>) {
        self.route_log_with_severity(id, source, LogSeverity::Info, text);
    }

    pub fn route_log_with_severity(
        &self,
        id: &str,
        source: LogSource,
        severity: LogSeverity,
        text: impl Into<String>,
    ) {
        let text = bounded_log_text(&text.into());
        if !self.try_append_log(id, text.clone()) {
            if source == LogSource::Bus {
                self.disclose_unknown_bus_id(id, "log");
            }
            return;
        }
        let sink = self.log_sink.lock().expect("log sink mutex poisoned");
        if let Some(sender) = sink.as_ref() {
            // Non-blocking: a full channel (redraw overdue) or a closed one
            // (no live TUI) both just mean this line never reaches the
            // scrollback - never worth blocking the caller (a bus-log
            // subscriber or output-reader task) over.
            let _ = sender.try_send(RoutedLogLine {
                participant: id.to_string(),
                source,
                severity,
                text,
            });
        }
    }

    fn disclose_unknown_bus_id(&self, id: &str, signal: &'static str) {
        const MAX_UNKNOWN_IDS: usize = 64;
        const MAX_UNKNOWN_ID_CHARS: usize = 128;
        let id = bounded_chars(id, MAX_UNKNOWN_ID_CHARS);
        let mut disclosed = self
            .unknown_bus_ids
            .lock()
            .expect("unknown bus id mutex poisoned");
        if disclosed.contains(&id) || disclosed.len() >= MAX_UNKNOWN_IDS {
            return;
        }
        disclosed.insert(id.clone());
        drop(disclosed);
        tracing::warn!(participant = %id, signal, "ignored bus traffic from an unplanned participant");
    }

    pub fn set_launch_command(&self, id: &str, command: ParticipantLaunchCommand) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.launch_command = Some(command);
        }
    }

    pub fn set_process_details(
        &self,
        id: &str,
        pid: Option<u32>,
        artifact_size_bytes: Option<u64>,
    ) {
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        if let Some(status) = snapshot.participants.get_mut(id) {
            status.pid = pid;
            status.artifact_size_bytes = artifact_size_bytes;
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> BoardSnapshot {
        self.inner.lock().expect("board mutex poisoned").clone()
    }
}
