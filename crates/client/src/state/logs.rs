use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use phoxal_cli_observation::{
    AttachmentEpoch, LogAnchor, LogRead, LogRow, LogWindow, StoreRevision, WindowDirection,
};

pub(crate) const LOG_CAPACITY: usize = 2_000;

pub(crate) struct LogStore {
    epoch: AttachmentEpoch,
    revision: StoreRevision,
    invalidation_pending: bool,
    rows: VecDeque<LogRow>,
    seen: BTreeSet<(
        std::time::SystemTime,
        Option<phoxal_cli_core::session::LogScope>,
        String,
        String,
    )>,
}

impl LogStore {
    pub fn new(epoch: AttachmentEpoch) -> Self {
        Self {
            epoch,
            revision: StoreRevision(0),
            invalidation_pending: false,
            rows: VecDeque::new(),
            seen: BTreeSet::new(),
        }
    }

    pub fn replace_epoch(&mut self, epoch: AttachmentEpoch) {
        self.epoch = epoch;
        self.revision = StoreRevision(self.revision.0.wrapping_add(1));
        self.invalidation_pending = false;
        self.rows.clear();
        self.seen.clear();
    }

    pub fn record(&mut self, epoch: AttachmentEpoch, row: LogRow) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        let key = (
            row.event_time,
            row.scope.clone(),
            row.participant.clone(),
            row.text.clone(),
        );
        if !self.seen.insert(key) {
            return None;
        }
        self.rows.push_back(row);
        while self.rows.len() > LOG_CAPACITY {
            if let Some(evicted) = self.rows.pop_front() {
                self.seen.remove(&(
                    evicted.event_time,
                    evicted.scope,
                    evicted.participant,
                    evicted.text,
                ));
            }
        }
        self.revision = StoreRevision(self.revision.0.wrapping_add(1));
        if self.invalidation_pending {
            None
        } else {
            self.invalidation_pending = true;
            Some(self.revision)
        }
    }

    pub fn install_snapshot(
        &mut self,
        epoch: AttachmentEpoch,
        scope: &phoxal_cli_core::session::LogScope,
        rows: impl IntoIterator<Item = LogRow>,
    ) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        let rows = rows.into_iter().collect::<Vec<_>>();
        if self
            .rows
            .iter()
            .filter(|row| row.scope.as_ref() == Some(scope))
            .eq(rows.iter())
        {
            return None;
        }
        let was_pending = self.invalidation_pending;
        self.rows.retain(|row| row.scope.as_ref() != Some(scope));
        self.seen = self
            .rows
            .iter()
            .map(|row| {
                (
                    row.event_time,
                    row.scope.clone(),
                    row.participant.clone(),
                    row.text.clone(),
                )
            })
            .collect();
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
                    .is_none_or(|participant| &row.participant == participant)
                    && query
                        .body
                        .filters
                        .minimum_severity
                        .is_none_or(|severity| row.severity >= severity)
                    && query
                        .body
                        .filters
                        .scope
                        .as_ref()
                        .is_none_or(|scope| row.scope.as_ref() == Some(scope))
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
    use phoxal_cli_core::identity::ExecutionId;
    use phoxal_cli_core::session::{LogSeverity, LogSource};
    use phoxal_cli_observation::{
        LogFilters, LogQuery, ObservationQuery, QueryToken, StoreRevision,
    };

    use super::*;

    fn epoch(graph_generation: u64) -> AttachmentEpoch {
        AttachmentEpoch {
            supervisor_generation: 1,
            execution_id: ExecutionId::mint(),
            graph_generation,
        }
    }

    fn row(index: usize) -> LogRow {
        LogRow {
            participant: "drive".into(),
            source: LogSource::Bus,
            severity: LogSeverity::Info,
            text: index.to_string(),
            event_time: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(index as u64),
            scope: None,
        }
    }

    #[test]
    fn high_rate_history_is_bounded_and_windowed_without_a_second_ring() {
        let epoch = epoch(0);
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

    #[test]
    fn stale_epoch_update_is_rejected() {
        let old = epoch(0);
        let new = AttachmentEpoch {
            graph_generation: 1,
            ..old
        };
        let mut store = LogStore::new(old);
        store.replace_epoch(new);
        assert_eq!(store.record(old, row(1)), None);
        assert!(store.record(new, row(2)).is_some());
    }
}
