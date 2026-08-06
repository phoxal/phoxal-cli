//! Snapshot ordering: subscribe first, query current, keep the highest
//! revision.
//!
//! The order matters and the rule is what makes it safe. A client installs the
//! stream *before* asking for the current document, so no revision can be
//! published into the gap between the two; the query's answer may then be older
//! than something already delivered, and keep-highest discards it without
//! needing to know which arrived first.
//!
//! Within one execution `revision` is monotonic and there is no generation to
//! compare first - a new execution is a new attachment, not a newer revision.

use phoxal_supervisor_api::Snapshot;
use tokio::sync::watch;

/// Keeps the highest revision seen, whatever order the stream and the query
/// deliver in.
#[derive(Debug, Default)]
pub struct SnapshotTracker {
    latest: Option<Snapshot>,
}

impl SnapshotTracker {
    #[must_use]
    pub const fn new() -> Self {
        Self { latest: None }
    }

    /// Offer a snapshot. Returns `true` when it was installed, which is to say
    /// when it is strictly newer than everything seen so far.
    ///
    /// An equal revision is dropped rather than reinstalled: within one
    /// execution the same revision is the same document, and reinstalling it
    /// would wake every observer for no change.
    pub fn accept(&mut self, snapshot: Snapshot) -> bool {
        match &self.latest {
            Some(current) if current.revision >= snapshot.revision => false,
            _ => {
                self.latest = Some(snapshot);
                true
            }
        }
    }

    #[must_use]
    pub const fn latest(&self) -> Option<&Snapshot> {
        self.latest.as_ref()
    }

    #[must_use]
    pub fn revision(&self) -> Option<u64> {
        self.latest.as_ref().map(|snapshot| snapshot.revision)
    }
}

/// The tracker plus the broadcast half a client observes.
///
/// `watch` is the right shape here: a snapshot is complete state, so a slow
/// observer wants the newest document rather than every intermediate one, and
/// `changed()` is cancellation-safe.
#[derive(Debug)]
pub struct SnapshotFeed {
    tracker: SnapshotTracker,
    sender: watch::Sender<Option<Snapshot>>,
}

impl SnapshotFeed {
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = watch::channel(None);
        Self {
            tracker: SnapshotTracker::new(),
            sender,
        }
    }

    /// Offer a snapshot, publishing it only when it is newer.
    pub fn accept(&mut self, snapshot: Snapshot) -> bool {
        if !self.tracker.accept(snapshot) {
            return false;
        }
        // `send_replace` rather than `send`: a feed with no observer yet must
        // still install the state, so a later subscriber sees the current
        // snapshot immediately instead of waiting for the next revision.
        self.sender.send_replace(self.tracker.latest().cloned());
        true
    }

    /// A new observer, which sees the current snapshot immediately.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Option<Snapshot>> {
        self.sender.subscribe()
    }

    #[must_use]
    pub const fn latest(&self) -> Option<&Snapshot> {
        self.tracker.latest()
    }
}

impl Default for SnapshotFeed {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_supervisor_api::{ExecutionMode, Lifecycle, Name, RobotIdentity};

    fn snapshot(revision: u64) -> Snapshot {
        Snapshot {
            revision,
            robot: RobotIdentity {
                id: Name::new("rover"),
                namespace: Name::new("demo"),
            },
            mode: ExecutionMode::Real,
            lifecycle: Lifecycle::Ready,
            startup: Vec::new(),
            processes: Vec::new(),
            failure: None,
        }
    }

    #[test]
    fn the_highest_revision_wins_whatever_order_it_arrives_in() {
        let mut tracker = SnapshotTracker::new();
        assert_eq!(tracker.revision(), None);

        // The stream delivers 5 while the current-query is still in flight.
        assert!(tracker.accept(snapshot(5)));
        // The query answers with the older 3 it captured before the subscribe:
        // dropped, so the client never regresses.
        assert!(!tracker.accept(snapshot(3)));
        assert_eq!(tracker.revision(), Some(5));
        // The same revision again is the same document.
        assert!(!tracker.accept(snapshot(5)));
        // And forward progress still installs.
        assert!(tracker.accept(snapshot(6)));
        assert_eq!(tracker.revision(), Some(6));
    }

    #[test]
    fn an_observer_sees_the_current_snapshot_and_only_real_changes() {
        let mut feed = SnapshotFeed::new();
        assert!(feed.accept(snapshot(2)));

        // Subscribing after the fact still yields the installed state.
        let mut observer = feed.subscribe();
        assert_eq!(
            observer.borrow_and_update().as_ref().map(|s| s.revision),
            Some(2)
        );

        assert!(!feed.accept(snapshot(1)));
        assert!(
            !observer.has_changed().unwrap(),
            "a stale revision woke an observer"
        );

        assert!(feed.accept(snapshot(3)));
        assert!(observer.has_changed().unwrap());
        assert_eq!(
            observer.borrow_and_update().as_ref().map(|s| s.revision),
            Some(3)
        );
    }
}
