//! Transport-neutral snapshot-plus-follow reconciliation owned by the
//! disposable attachment client.

use std::collections::VecDeque;
use std::time::Duration;

use phoxal_api::runtime::telemetry::Cursor;

pub trait Sequenced {
    fn cursor(&self) -> Cursor;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome<T> {
    Buffered,
    Append(T),
    Installed { snapshot: Vec<T>, replay: Vec<T> },
    Requery,
}

#[derive(Debug)]
pub struct Reconciler<T> {
    cursor: Option<Cursor>,
    buffer: VecDeque<T>,
    buffer_capacity: usize,
    querying: bool,
}

impl<T: Sequenced> Reconciler<T> {
    #[must_use]
    pub fn new(buffer_capacity: usize) -> Self {
        Self {
            cursor: None,
            buffer: VecDeque::with_capacity(buffer_capacity),
            buffer_capacity: buffer_capacity.max(1),
            querying: true,
        }
    }

    pub fn begin_query(&mut self) {
        self.querying = true;
    }

    #[cfg(test)]
    pub fn local_drop(&mut self) -> ReconcileOutcome<T> {
        self.begin_query();
        self.buffer.clear();
        ReconcileOutcome::Requery
    }

    pub fn follow(&mut self, item: T) -> ReconcileOutcome<T> {
        if self.querying {
            if self.buffer.len() == self.buffer_capacity {
                self.buffer.pop_front();
            }
            self.buffer.push_back(item);
            return ReconcileOutcome::Buffered;
        }
        let next = item.cursor();
        let Some(installed) = &self.cursor else {
            self.begin_query();
            return ReconcileOutcome::Requery;
        };
        if next.sequence != installed.sequence.saturating_add(1) {
            self.begin_query();
            self.buffer.clear();
            return ReconcileOutcome::Requery;
        }
        self.cursor = Some(next);
        ReconcileOutcome::Append(item)
    }

    pub fn install(&mut self, cursor: Cursor, snapshot: Vec<T>) -> ReconcileOutcome<T> {
        let mut installed = cursor;
        let mut replay = Vec::new();
        while let Some(item) = self.buffer.pop_front() {
            let next = item.cursor();
            if next.sequence < installed.sequence {
                continue;
            }
            if next.sequence == installed.sequence {
                continue;
            }
            if next.sequence != installed.sequence.saturating_add(1) {
                self.begin_query();
                self.buffer.clear();
                return ReconcileOutcome::Requery;
            }
            installed = next;
            replay.push(item);
        }
        self.cursor = Some(installed);
        self.querying = false;
        ReconcileOutcome::Installed { snapshot, replay }
    }
}

/// Small bounded delay used when a retained-feed consumer has to issue a new
/// snapshot query. It prevents a persistent sequence hole from becoming a hot
/// retry loop while still keeping recovery responsive.
#[derive(Debug, Clone)]
pub struct RetryBackoff {
    initial: Duration,
    maximum: Duration,
    next: Duration,
}

impl RetryBackoff {
    #[must_use]
    pub fn new(initial: Duration, maximum: Duration) -> Self {
        let initial = initial.min(maximum);
        Self {
            initial,
            maximum,
            next: initial,
        }
    }

    pub fn reset(&mut self) {
        self.next = self.initial;
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.maximum);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Item(Cursor);

    impl Sequenced for Item {
        fn cursor(&self) -> Cursor {
            self.0.clone()
        }
    }

    fn item(sequence: u64) -> Item {
        Item(Cursor { sequence })
    }

    #[test]
    fn subscribe_first_buffer_discards_covered_and_replays_newer() {
        let mut reconciler = Reconciler::new(4);
        assert_eq!(reconciler.follow(item(2)), ReconcileOutcome::Buffered);
        assert_eq!(reconciler.follow(item(3)), ReconcileOutcome::Buffered);
        assert_eq!(
            reconciler.install(Cursor { sequence: 2 }, vec![item(1), item(2)]),
            ReconcileOutcome::Installed {
                snapshot: vec![item(1), item(2)],
                replay: vec![item(3)],
            }
        );
        assert_eq!(
            reconciler.follow(item(4)),
            ReconcileOutcome::Append(item(4))
        );
    }

    #[test]
    fn snapshot_discards_every_covered_follow_record_and_keeps_following() {
        let mut reconciler = Reconciler::new(8);
        assert_eq!(reconciler.follow(item(5)), ReconcileOutcome::Buffered);
        assert_eq!(reconciler.follow(item(6)), ReconcileOutcome::Buffered);

        assert_eq!(
            reconciler.install(Cursor { sequence: 6 }, vec![item(5), item(6)]),
            ReconcileOutcome::Installed {
                snapshot: vec![item(5), item(6)],
                replay: Vec::new(),
            }
        );
        assert_eq!(
            reconciler.follow(item(7)),
            ReconcileOutcome::Append(item(7))
        );
    }

    #[test]
    fn generation_gap_and_local_drop_require_requery() {
        for next in [item(0), item(3)] {
            let mut reconciler = Reconciler::new(4);
            reconciler.install(Cursor { sequence: 1 }, Vec::new());
            assert_eq!(reconciler.follow(next), ReconcileOutcome::Requery);
        }
        let mut reconciler = Reconciler::<Item>::new(4);
        assert_eq!(reconciler.local_drop(), ReconcileOutcome::Requery);
    }

    #[test]
    fn bounded_buffer_overflow_drops_oldest_and_installs_when_snapshot_covers_it() {
        let mut reconciler = Reconciler::new(1);
        assert_eq!(reconciler.follow(item(1)), ReconcileOutcome::Buffered);
        assert_eq!(reconciler.follow(item(2)), ReconcileOutcome::Buffered);
        assert_eq!(
            reconciler.install(Cursor { sequence: 1 }, vec![item(1)]),
            ReconcileOutcome::Installed {
                snapshot: vec![item(1)],
                replay: vec![item(2)],
            }
        );
    }

    #[test]
    fn overflow_requeries_only_when_surviving_buffer_exposes_a_hole() {
        let mut reconciler = Reconciler::new(1);
        assert_eq!(reconciler.follow(item(2)), ReconcileOutcome::Buffered);
        assert_eq!(reconciler.follow(item(3)), ReconcileOutcome::Buffered);
        assert_eq!(
            reconciler.install(Cursor { sequence: 1 }, vec![item(1)]),
            ReconcileOutcome::Requery
        );
    }

    #[test]
    fn an_older_buffered_record_is_covered_by_the_new_snapshot() {
        let mut reconciler = Reconciler::new(4);
        assert_eq!(reconciler.follow(item(1)), ReconcileOutcome::Buffered);
        assert_eq!(
            reconciler.install(Cursor { sequence: 4 }, vec![item(4)]),
            ReconcileOutcome::Installed {
                snapshot: vec![item(4)],
                replay: Vec::new(),
            }
        );
    }

    #[test]
    fn retry_backoff_is_bounded_and_resettable() {
        let mut backoff = RetryBackoff::new(Duration::from_millis(10), Duration::from_millis(25));
        assert_eq!(backoff.next_delay(), Duration::from_millis(10));
        assert_eq!(backoff.next_delay(), Duration::from_millis(20));
        assert_eq!(backoff.next_delay(), Duration::from_millis(25));
        assert_eq!(backoff.next_delay(), Duration::from_millis(25));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(10));
    }
}
