use std::collections::VecDeque;
use std::sync::Arc;

use phoxal_cli_observation::{
    AttachmentEpoch, LogAnchor, LogRead, LogRow, LogWindow, StoreRevision, WindowDirection,
};

use super::contains_ascii_case_insensitive;

pub(crate) const LOG_CAPACITY: usize = 2_000;

pub(crate) struct LogStore {
    epoch: AttachmentEpoch,
    revision: StoreRevision,
    invalidation_pending: bool,
    rows: VecDeque<LogRow>,
}

impl LogStore {
    pub fn new(epoch: AttachmentEpoch) -> Self {
        Self {
            epoch,
            revision: StoreRevision(0),
            invalidation_pending: false,
            rows: VecDeque::new(),
        }
    }

    pub fn record(&mut self, epoch: AttachmentEpoch, row: LogRow) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        self.rows.push_back(row);
        while self.rows.len() > LOG_CAPACITY {
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

    /// Install one reconciled page as the complete retained history.
    ///
    /// There is exactly one collector - the supervisor - and one execution, so
    /// a page replaces the history rather than one robot's slice of it.
    pub fn install_snapshot(
        &mut self,
        epoch: AttachmentEpoch,
        rows: impl IntoIterator<Item = LogRow>,
    ) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        let rows = rows.into_iter().collect::<Vec<_>>();
        if self.rows.iter().eq(rows.iter()) {
            return None;
        }
        let was_pending = self.invalidation_pending;
        self.rows.clear();
        for row in rows {
            let _ = self.record(epoch, row);
        }
        self.revision = StoreRevision(self.revision.0.wrapping_add(1));
        self.invalidation_pending = true;
        (!was_pending).then_some(self.revision)
    }

    pub fn read(&mut self, query: LogRead) -> LogWindow {
        if query.epoch == self.epoch {
            self.invalidation_pending = false;
        }
        let mut rows = self
            .rows
            .iter()
            .filter(|row| {
                query
                    .body
                    .filters
                    .participant
                    .as_ref()
                    .is_none_or(|participant| {
                        contains_ascii_case_insensitive(&row.participant, participant)
                    })
                    && query
                        .body
                        .filters
                        .minimum_severity
                        .is_none_or(|severity| row.severity >= severity)
                    && match query.body.anchor {
                        LogAnchor::Before(at) => row.event_time < at,
                        LogAnchor::After(at) => row.event_time > at,
                        LogAnchor::Latest => true,
                    }
            })
            .cloned()
            .collect::<Vec<_>>();
        if query.body.direction == WindowDirection::Backward {
            rows.reverse();
        }
        rows.truncate(query.body.limit.min(LOG_CAPACITY));
        LogWindow {
            epoch: self.epoch,
            revision: self.revision,
            token: query.token,
            rows: Arc::from(rows),
        }
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_observation::{
        LogFilters, LogQuery, ObservationQuery, QueryToken, StoreRevision,
    };
    use phoxal_cli_observation::{LogSeverity, LogSource};
    use phoxal_runtime_contract::identity::ExecutionId;

    use super::*;

    fn epoch() -> AttachmentEpoch {
        AttachmentEpoch::new(ExecutionId::mint())
    }

    fn row(index: usize) -> LogRow {
        LogRow {
            participant: "drive".into(),
            source: LogSource::Bus,
            severity: LogSeverity::Info,
            text: index.to_string(),
            event_time: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(index as u64),
        }
    }

    #[test]
    fn high_rate_history_is_bounded_and_windowed_without_a_second_ring() {
        let epoch = epoch();
        let mut store = LogStore::new(epoch);
        for index in 0..(LOG_CAPACITY * 3) {
            store.record(epoch, row(index));
        }
        let window = store.read(ObservationQuery {
            epoch,
            observed_revision: StoreRevision(0),
            token: QueryToken(7),
            body: LogQuery {
                filters: LogFilters::default(),
                anchor: LogAnchor::Latest,
                direction: WindowDirection::Backward,
                limit: usize::MAX,
            },
        });
        assert_eq!(window.rows.len(), LOG_CAPACITY);
        assert_eq!(window.token, QueryToken(7));
        assert_eq!(window.rows[0].text, (LOG_CAPACITY * 3 - 1).to_string());
    }

    /// A new execution is a new attachment, so a row stamped with the previous
    /// one belongs to a run that has ended.
    #[test]
    fn a_row_from_a_previous_execution_is_rejected() {
        let old = epoch();
        let new = epoch();
        let mut store = LogStore::new(new);
        assert_eq!(store.record(old, row(1)), None);
        assert!(store.record(new, row(2)).is_some());
    }

    #[test]
    fn content_identical_reconciled_records_are_preserved() {
        let epoch = epoch();
        let mut store = LogStore::new(epoch);
        let repeated = row(1);
        assert!(store.record(epoch, repeated.clone()).is_some());
        store.read(ObservationQuery {
            epoch,
            observed_revision: StoreRevision(0),
            token: QueryToken(1),
            body: LogQuery {
                filters: LogFilters::default(),
                anchor: LogAnchor::Latest,
                direction: WindowDirection::Backward,
                limit: usize::MAX,
            },
        });
        assert!(store.record(epoch, repeated).is_some());
        let window = store.read(ObservationQuery {
            epoch,
            observed_revision: StoreRevision(0),
            token: QueryToken(2),
            body: LogQuery {
                filters: LogFilters::default(),
                anchor: LogAnchor::Latest,
                direction: WindowDirection::Backward,
                limit: usize::MAX,
            },
        });
        assert_eq!(window.rows.len(), 2);
    }

    #[test]
    fn participant_filter_is_case_insensitive_and_incremental() {
        let epoch = epoch();
        let mut store = LogStore::new(epoch);
        store.record(epoch, row(1));
        let window = store.read(ObservationQuery {
            epoch,
            observed_revision: StoreRevision(0),
            token: QueryToken(8),
            body: LogQuery {
                filters: LogFilters {
                    participant: Some("DRI".to_string()),
                    ..LogFilters::default()
                },
                anchor: LogAnchor::Latest,
                direction: WindowDirection::Forward,
                limit: 10,
            },
        });
        assert_eq!(window.rows.len(), 1);
    }
}
