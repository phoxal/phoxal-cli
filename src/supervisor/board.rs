//! Concurrent session-board state and heartbeat tracking.

use super::{
    BoardSnapshot, LogSeverity, LogSource, ParticipantLaunchCommand, ParticipantState,
    ParticipantStatus, RoutedLogLine, bounded_chars, bounded_log_text,
};
use phoxal_api::v1 as api;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::human;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub struct BoardBackend {
    inner: Arc<Mutex<BoardSnapshot>>,
    /// Wall-clock instant each participant's presence/heartbeat was last
    /// observed. Separate from `inner` because it is process-local bookkeeping
    /// (an `Instant`, not serializable) rather than board state; an entry
    /// appears here from the FIRST heartbeat of any readiness (including the
    /// pre-`#[setup]` `Initializing` beacon), so this alone cannot tell "never
    /// checked in" apart from "checked in once and then went silent mid-setup"
    /// - `ready_once` (below) makes that distinction.
    pub(crate) heartbeats: Arc<Mutex<BTreeMap<String, Instant>>>,
    /// Participant ids observed at `Readiness::Ready` at least once during
    /// their current incarnation. `mark_stale_heartbeats` only applies its 5s
    /// liveness sweep to ids in this set: a participant that has never reached
    /// `Ready` is still within a legitimate `#[setup]` (the runner publishes
    /// only one `Initializing` heartbeat before `#[setup]` runs, then nothing
    /// until the post-setup `Ready` beacon, so a slow setup is heartbeat-silent
    /// by design) and is bounded by its startup stage's timeout instead, not
    /// this sweep. Cleared alongside `heartbeats` by
    /// `reset_participant_liveness` on every (re)spawn so a fresh incarnation
    /// must earn `Ready` again before the sweep applies to it.
    pub(crate) ready_once: Arc<Mutex<BTreeSet<String>>>,
    /// Participants whose process lifecycle belongs to an external owner
    /// (currently Webots). Their heartbeat loss is observational: it degrades
    /// the board entry but cannot become session-teardown authority, and a
    /// later heartbeat may restore the current readiness.
    recoverable_presence: Arc<Mutex<BTreeSet<String>>>,
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

    /// Record an observed `presence/heartbeat` for `id`, driving its board
    /// state from OBSERVED readiness rather than any spawn-time assumption.
    /// Ignored for a supervised participant already in a terminal state
    /// (`Failed`, `Stopped`) - a heartbeat that was in flight when the process
    /// was independently torn down must not resurrect it. Externally managed
    /// participants registered with [`Self::mark_presence_recoverable`] may
    /// recover from `Failed`/`Degraded` when their owner recreates them.
    pub fn record_heartbeat(&self, id: &str, readiness: api::presence::Readiness) {
        let recoverable = self
            .recoverable_presence
            .lock()
            .expect("recoverable presence mutex poisoned")
            .contains(id);
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        let Some(status) = snapshot.participants.get_mut(id) else {
            drop(snapshot);
            self.disclose_unknown_bus_id(id, "heartbeat");
            return;
        };
        if status.state == ParticipantState::Stopped
            || (status.state == ParticipantState::Failed && !recoverable)
        {
            return;
        }
        let was_unobserved = recoverable
            && matches!(
                status.state,
                ParticipantState::Degraded | ParticipantState::Failed
            );
        status.state = match readiness {
            api::presence::Readiness::Ready => ParticipantState::Ready,
            api::presence::Readiness::Degraded => ParticipantState::Degraded,
            api::presence::Readiness::Failed => ParticipantState::Failed,
            api::presence::Readiness::NotStarted | api::presence::Readiness::Initializing => {
                ParticipantState::Starting
            }
        };
        if was_unobserved
            && matches!(
                status.state,
                ParticipantState::Ready | ParticipantState::Starting
            )
        {
            status.note = Some("Webots-managed participant observed again".to_string());
        }
        drop(snapshot);
        self.heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .insert(id.to_string(), Instant::now());
        if matches!(readiness, api::presence::Readiness::Ready) {
            self.ready_once
                .lock()
                .expect("ready-once mutex poisoned")
                .insert(id.to_string());
        }
    }

    /// Mark `id` as externally managed presence. Its silence is recoverable
    /// observation state, not proof that the developer session must end.
    pub fn mark_presence_recoverable(&self, id: impl Into<String>) {
        self.recoverable_presence
            .lock()
            .expect("recoverable presence mutex poisoned")
            .insert(id.into());
    }

    /// Clear a participant's staleness bookkeeping - its last-heartbeat
    /// timestamp and its "has been `Ready`" bit - so a fresh incarnation
    /// starts with a clean liveness clock and must earn `Ready` again before
    /// `mark_stale_heartbeats` will apply to it. Called from `spawn_child` on
    /// every (re)spawn: the initial spawn, a crash restart, a `swap`, and a
    /// `resume` after `release` all funnel through it. Without this, a
    /// previously-`Ready`-then-crashed participant's stale pre-crash timestamp
    /// would survive the reset to `Starting` and could immediately re-`Fail`
    /// the freshly respawned process before it had a chance to publish its
    /// first heartbeat.
    pub fn reset_participant_liveness(&self, id: &str) {
        self.heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .remove(id);
        self.ready_once
            .lock()
            .expect("ready-once mutex poisoned")
            .remove(id);
    }

    /// Mark any participant that has reached `Ready` at least once this
    /// incarnation and has gone silent for longer than `stale_after`.
    /// CLI-supervised participants become `Failed`; externally managed
    /// participants become recoverably `Degraded` because pause/deletion is
    /// owned by Webots and may be followed by recreation.
    ///
    /// Deliberately excludes a participant that has never been observed
    /// `Ready` (only ever `Starting`, e.g. mid a legitimately slow
    /// `#[setup]`) - see the `ready_once` field docs. That case is bounded by
    /// its startup stage's timeout, not this sweep.
    pub fn mark_stale_heartbeats(&self, stale_after: Duration) {
        let now = Instant::now();
        let stale_ids: Vec<(String, bool)> = {
            let heartbeats = self.heartbeats.lock().expect("heartbeat mutex poisoned");
            let ready_once = self.ready_once.lock().expect("ready-once mutex poisoned");
            let recoverable = self
                .recoverable_presence
                .lock()
                .expect("recoverable presence mutex poisoned");
            heartbeats
                .iter()
                .filter(|(id, seen)| {
                    ready_once.contains(id.as_str()) && now.duration_since(**seen) > stale_after
                })
                .map(|(id, _)| (id.clone(), recoverable.contains(id)))
                .collect()
        };
        if stale_ids.is_empty() {
            return;
        }
        let mut snapshot = self.inner.lock().expect("board mutex poisoned");
        for (id, recoverable) in stale_ids {
            let Some(status) = snapshot.participants.get_mut(&id) else {
                continue;
            };
            if matches!(
                status.state,
                ParticipantState::Failed | ParticipantState::Stopped
            ) {
                continue;
            }
            if recoverable {
                status.state = ParticipantState::Degraded;
                status.note = Some(format!(
                    "Webots-managed participant not observed for over {}; waiting for its owner",
                    human::duration(stale_after)
                ));
            } else {
                status.state = ParticipantState::Failed;
                status.note = Some(format!(
                    "heartbeat stopped: no presence/heartbeat observed for over {}",
                    human::duration(stale_after)
                ));
            }
        }
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

    /// Session-only receive instants for the TUI. This stays outside the
    /// persisted BoardSnapshot and its stable plain/JSON representations.
    #[must_use]
    pub fn heartbeat_snapshot(&self) -> BTreeMap<String, Instant> {
        self.heartbeats
            .lock()
            .expect("heartbeat mutex poisoned")
            .clone()
    }
}
