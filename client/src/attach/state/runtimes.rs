use std::collections::VecDeque;
use std::sync::Arc;

use phoxal_cli_observation::{
    AttachmentEpoch, RuntimeRead, RuntimeRow, RuntimeWindow, StoreRevision, WindowDirection,
};

const CAPACITY: usize = 4_096;

pub(crate) struct RuntimeStore {
    epoch: AttachmentEpoch,
    revision: StoreRevision,
    invalidation_pending: bool,
    capacity_evictions: u64,
    rows: VecDeque<RuntimeRow>,
}

impl RuntimeStore {
    pub fn new(epoch: AttachmentEpoch) -> Self {
        Self {
            epoch,
            revision: StoreRevision(0),
            invalidation_pending: false,
            capacity_evictions: 0,
            rows: VecDeque::new(),
        }
    }

    /// Install one reconciled page as the complete retained history.
    pub fn install_snapshot(
        &mut self,
        epoch: AttachmentEpoch,
        samples: impl IntoIterator<Item = phoxal_cli_observation::RuntimePerformanceSample>,
        status: phoxal_cli_observation::RuntimeFeedStatus,
    ) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        self.rows.clear();
        self.capacity_evictions = status.capacity_evictions;
        self.rows
            .extend(samples.into_iter().map(|sample| RuntimeRow {
                sample,
                capacity_evictions: self.capacity_evictions,
            }));
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

    pub fn record(
        &mut self,
        epoch: AttachmentEpoch,
        sample: phoxal_cli_observation::RuntimePerformanceSample,
        status: phoxal_cli_observation::RuntimeFeedStatus,
    ) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        self.capacity_evictions = self.capacity_evictions.max(status.capacity_evictions);
        self.rows.push_back(RuntimeRow {
            sample,
            capacity_evictions: self.capacity_evictions,
        });
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

    pub fn read(&mut self, query: RuntimeRead) -> RuntimeWindow {
        if query.epoch == self.epoch {
            self.invalidation_pending = false;
        }
        let mut rows = self
            .rows
            .iter()
            .filter(|row| {
                query
                    .body
                    .participant
                    .as_ref()
                    .is_none_or(|participant| &row.sample.participant_id == participant)
            })
            .cloned()
            .collect::<Vec<_>>();
        if query.body.direction == WindowDirection::Backward {
            rows.reverse();
        }
        rows.truncate(query.body.limit.min(CAPACITY));
        RuntimeWindow {
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
    use phoxal_cli_observation::{
        ObservationQuery, QueryToken, RuntimeFeedStatus, RuntimePerformanceSample, RuntimeQuery,
        StoreRevision,
    };

    use super::*;

    fn sample(id: &str, sequence: u64) -> RuntimePerformanceSample {
        RuntimePerformanceSample {
            sequence,
            participant_id: id.to_string(),
            truncated: 0,
            window_ns: 1,
            step: None,
            topics: std::sync::Arc::new(Vec::new()),
            overflow: None,
        }
    }

    fn read(store: &mut RuntimeStore, epoch: AttachmentEpoch) -> RuntimeWindow {
        store.read(ObservationQuery {
            epoch,
            observed_revision: StoreRevision(0),
            token: QueryToken(1),
            body: RuntimeQuery {
                participant: None,
                direction: WindowDirection::Backward,
                limit: usize::MAX,
            },
        })
    }

    /// One execution has one collector, so a reconciled page IS the retained
    /// history rather than one robot's slice of it.
    #[test]
    fn a_reconciled_page_replaces_the_history_and_follows_extend_it() {
        let epoch = AttachmentEpoch::new(ExecutionId::mint());
        let status = RuntimeFeedStatus {
            capacity_evictions: 3,
        };
        let mut store = RuntimeStore::new(epoch);
        assert!(
            store
                .install_snapshot(epoch, [sample("drive", 1)], status)
                .is_some()
        );
        store.record(epoch, sample("brain", 2), status);
        let window = read(&mut store, epoch);
        assert_eq!(window.rows.len(), 2);
        assert!(
            window.rows.iter().all(|row| row.capacity_evictions == 3),
            "every retained row reports the collector's eviction count"
        );

        // The read above cleared the pending invalidation, so the next page
        // announces a fresh revision - and replaces the history rather than
        // appending to it.
        assert!(
            store
                .install_snapshot(epoch, [sample("drive", 4)], status)
                .is_some()
        );
        assert_eq!(read(&mut store, epoch).rows.len(), 1);
    }

    /// A new execution is a new attachment: nothing from the previous one may
    /// be spliced onto it.
    #[test]
    fn a_sample_from_a_previous_execution_is_rejected() {
        let old = AttachmentEpoch::new(ExecutionId::mint());
        let new = AttachmentEpoch::new(ExecutionId::mint());
        let status = RuntimeFeedStatus::default();
        let mut store = RuntimeStore::new(new);
        assert_eq!(store.record(old, sample("drive", 1), status), None);
        assert!(store.record(new, sample("drive", 2), status).is_some());
    }
}
