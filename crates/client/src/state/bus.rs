use std::collections::VecDeque;
use std::sync::Arc;

use phoxal_cli_observation::{
    AttachmentEpoch, BusRead, BusRow, BusWindow, StoreRevision, WindowDirection,
};

use super::contains_ascii_case_insensitive;

// One maximal 256-topic sample per second for a full minute, plus headroom.
const CAPACITY: usize = 16_384;

pub(crate) struct BusStore {
    epoch: AttachmentEpoch,
    revision: StoreRevision,
    invalidation_pending: bool,
    rows: VecDeque<BusRow>,
}

impl BusStore {
    pub fn new(epoch: AttachmentEpoch) -> Self {
        Self {
            epoch,
            revision: StoreRevision(0),
            invalidation_pending: false,
            rows: VecDeque::new(),
        }
    }

    pub fn replace_epoch(&mut self, epoch: AttachmentEpoch) {
        self.epoch = epoch;
        self.revision = StoreRevision(self.revision.0.wrapping_add(1));
        self.invalidation_pending = false;
        self.rows.clear();
    }

    pub fn record(
        &mut self,
        epoch: AttachmentEpoch,
        rows: impl IntoIterator<Item = BusRow>,
    ) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        self.rows.extend(rows);
        while self.rows.len() > CAPACITY {
            self.rows.pop_front();
        }
        self.revision = StoreRevision(self.revision.0.wrapping_add(1));
        if self.invalidation_pending {
            None
        } else {
            self.invalidation_pending = true;
            Some(self.revision)
        }
    }

    pub fn install(
        &mut self,
        epoch: AttachmentEpoch,
        scope: &phoxal_cli_observation::RobotScope,
        rows: impl IntoIterator<Item = BusRow>,
    ) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        self.rows.retain(|row| &row.scope != scope);
        self.rows.extend(rows);
        while self.rows.len() > CAPACITY {
            self.rows.pop_front();
        }
        self.revision = StoreRevision(self.revision.0.wrapping_add(1));
        if self.invalidation_pending {
            None
        } else {
            self.invalidation_pending = true;
            Some(self.revision)
        }
    }

    pub fn read(&mut self, query: BusRead) -> BusWindow {
        if query.epoch == self.epoch {
            self.invalidation_pending = false;
        }
        let mut rows = self
            .rows
            .iter()
            .filter(|row| {
                query.body.topic.as_ref().is_none_or(|filter| {
                    contains_ascii_case_insensitive(&row.topic, filter)
                        || contains_ascii_case_insensitive(&row.participant, filter)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if query.body.direction == WindowDirection::Backward {
            rows.reverse();
        }
        rows.truncate(query.body.limit.min(CAPACITY));
        BusWindow {
            epoch: self.epoch,
            revision: self.revision,
            token: query.token,
            rows: Arc::from(rows),
        }
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::identity::ExecutionId;
    use phoxal_cli_observation::RobotScope;
    use phoxal_cli_observation::{BusQuery, ObservationQuery, QueryToken, StoreRevision};

    use super::*;

    fn epoch() -> AttachmentEpoch {
        AttachmentEpoch {
            supervisor_generation: 1,
            execution_id: ExecutionId::mint(),
            graph_generation: 1,
        }
    }

    fn row(scope: &RobotScope, observed_at: std::time::Instant, count: u64) -> BusRow {
        BusRow {
            scope: scope.clone(),
            observed_at,
            topic: "v1/drive/state".to_string(),
            participant: "drive".to_string(),
            rate_hz: count as f32,
            count,
            aggregate_overflow: false,
            topics_truncated: 0,
            throughput_msg_s: count as f32,
            window_ns: 1,
        }
    }

    #[test]
    fn one_invalidation_covers_all_updates_until_the_window_is_read() {
        let epoch = epoch();
        let scope = RobotScope {
            namespace: "lab".to_string(),
            robot_id: "rover".to_string(),
        };
        let now = std::time::Instant::now();
        let mut store = BusStore::new(epoch);
        assert!(store.record(epoch, [row(&scope, now, 1)]).is_some());
        assert!(
            store
                .record(
                    epoch,
                    [row(&scope, now + std::time::Duration::from_secs(1), 2)]
                )
                .is_none()
        );
        let window = store.read(ObservationQuery {
            epoch,
            observed_revision: StoreRevision(0),
            token: QueryToken(1),
            body: BusQuery {
                topic: None,
                direction: WindowDirection::Backward,
                limit: usize::MAX,
            },
        });
        assert_eq!(window.rows.len(), 2);
        assert!(
            store
                .record(
                    epoch,
                    [row(&scope, now + std::time::Duration::from_secs(2), 3)]
                )
                .is_some()
        );
    }

    #[test]
    fn retained_snapshot_installs_every_window_for_one_scope() {
        let epoch = epoch();
        let scope = RobotScope {
            namespace: "lab".to_string(),
            robot_id: "rover".to_string(),
        };
        let now = std::time::Instant::now();
        let mut store = BusStore::new(epoch);
        assert!(
            store
                .install(
                    epoch,
                    &scope,
                    [
                        row(&scope, now, 1),
                        row(&scope, now + std::time::Duration::from_secs(1), 2),
                        row(&scope, now + std::time::Duration::from_secs(2), 3),
                    ],
                )
                .is_some()
        );
        let window = store.read(ObservationQuery {
            epoch,
            observed_revision: StoreRevision(0),
            token: QueryToken(2),
            body: BusQuery {
                topic: None,
                direction: WindowDirection::Forward,
                limit: usize::MAX,
            },
        });
        assert_eq!(window.rows.len(), 3);
        assert_eq!(window.rows[0].throughput_msg_s, 1.0);
        assert_eq!(window.rows[2].throughput_msg_s, 3.0);
    }

    #[test]
    fn a_stale_query_does_not_consume_the_current_epoch_invalidation() {
        let old = epoch();
        let current = AttachmentEpoch {
            graph_generation: old.graph_generation + 1,
            ..old
        };
        let scope = RobotScope {
            namespace: "lab".to_string(),
            robot_id: "rover".to_string(),
        };
        let now = std::time::Instant::now();
        let mut store = BusStore::new(old);
        store.replace_epoch(current);
        assert!(store.record(current, [row(&scope, now, 1)]).is_some());
        let _ = store.read(ObservationQuery {
            epoch: old,
            observed_revision: StoreRevision(0),
            token: QueryToken(3),
            body: BusQuery {
                topic: None,
                direction: WindowDirection::Forward,
                limit: usize::MAX,
            },
        });
        assert!(
            store
                .record(
                    current,
                    [row(&scope, now + std::time::Duration::from_secs(1), 2)]
                )
                .is_none()
        );
    }

    #[test]
    fn bus_filter_matches_topic_or_producer_case_insensitively() {
        let epoch = epoch();
        let scope = RobotScope {
            namespace: "lab".to_string(),
            robot_id: "rover".to_string(),
        };
        let mut store = BusStore::new(epoch);
        store.record(epoch, [row(&scope, std::time::Instant::now(), 1)]);
        for filter in ["DRIVE/STA", "DRI"] {
            let window = store.read(ObservationQuery {
                epoch,
                observed_revision: StoreRevision(0),
                token: QueryToken(4),
                body: BusQuery {
                    topic: Some(filter.to_string()),
                    direction: WindowDirection::Forward,
                    limit: 10,
                },
            });
            assert_eq!(window.rows.len(), 1, "{filter} should match");
        }
    }
}
