//! Transport-neutral snapshot-plus-follow reconciliation owned by the
//! disposable attachment client.

use std::collections::VecDeque;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub generation: String,
    pub sequence: u64,
}

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
        if next.generation != installed.generation
            || next.sequence != installed.sequence.saturating_add(1)
        {
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
            if next.generation != installed.generation {
                self.begin_query();
                self.buffer.clear();
                return ReconcileOutcome::Requery;
            }
            if next.sequence <= installed.sequence {
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

    fn item(generation: &str, sequence: u64) -> Item {
        Item(Cursor {
            generation: generation.to_string(),
            sequence,
        })
    }

    #[test]
    fn subscribe_first_buffer_discards_covered_and_replays_newer() {
        let mut reconciler = Reconciler::new(4);
        assert_eq!(reconciler.follow(item("a", 2)), ReconcileOutcome::Buffered);
        assert_eq!(reconciler.follow(item("a", 3)), ReconcileOutcome::Buffered);
        assert_eq!(
            reconciler.install(
                Cursor {
                    generation: "a".to_string(),
                    sequence: 2,
                },
                vec![item("a", 1), item("a", 2)]
            ),
            ReconcileOutcome::Installed {
                snapshot: vec![item("a", 1), item("a", 2)],
                replay: vec![item("a", 3)],
            }
        );
        assert_eq!(
            reconciler.follow(item("a", 4)),
            ReconcileOutcome::Append(item("a", 4))
        );
    }

    #[test]
    fn generation_gap_and_local_drop_require_requery() {
        for next in [item("b", 1), item("a", 3)] {
            let mut reconciler = Reconciler::new(4);
            reconciler.install(
                Cursor {
                    generation: "a".to_string(),
                    sequence: 1,
                },
                Vec::new(),
            );
            assert_eq!(reconciler.follow(next), ReconcileOutcome::Requery);
        }
        let mut reconciler = Reconciler::<Item>::new(4);
        assert_eq!(reconciler.local_drop(), ReconcileOutcome::Requery);
    }

    #[test]
    fn bounded_buffer_overflow_drops_oldest_and_installs_when_snapshot_covers_it() {
        let mut reconciler = Reconciler::new(1);
        assert_eq!(reconciler.follow(item("a", 1)), ReconcileOutcome::Buffered);
        assert_eq!(reconciler.follow(item("a", 2)), ReconcileOutcome::Buffered);
        assert_eq!(
            reconciler.install(
                Cursor {
                    generation: "a".to_string(),
                    sequence: 1,
                },
                vec![item("a", 1)]
            ),
            ReconcileOutcome::Installed {
                snapshot: vec![item("a", 1)],
                replay: vec![item("a", 2)],
            }
        );
    }

    #[test]
    fn overflow_requeries_only_when_surviving_buffer_exposes_a_hole() {
        let mut reconciler = Reconciler::new(1);
        assert_eq!(reconciler.follow(item("a", 2)), ReconcileOutcome::Buffered);
        assert_eq!(reconciler.follow(item("a", 3)), ReconcileOutcome::Buffered);
        assert_eq!(
            reconciler.install(
                Cursor {
                    generation: "a".to_string(),
                    sequence: 1,
                },
                vec![item("a", 1)]
            ),
            ReconcileOutcome::Requery
        );
    }

    #[test]
    fn generation_change_buffered_during_query_requires_requery() {
        let mut reconciler = Reconciler::new(4);
        assert_eq!(reconciler.follow(item("b", 1)), ReconcileOutcome::Buffered);
        assert_eq!(
            reconciler.install(
                Cursor {
                    generation: "a".to_string(),
                    sequence: 4,
                },
                vec![item("a", 4)]
            ),
            ReconcileOutcome::Requery
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
