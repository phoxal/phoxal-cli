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
    capacity_evictions: std::collections::BTreeMap<phoxal_cli_core::session::RobotScope, u64>,
    rows: VecDeque<RuntimeRow>,
}

impl RuntimeStore {
    pub fn new(epoch: AttachmentEpoch) -> Self {
        Self {
            epoch,
            revision: StoreRevision(0),
            invalidation_pending: false,
            capacity_evictions: std::collections::BTreeMap::new(),
            rows: VecDeque::new(),
        }
    }

    pub fn replace_epoch(&mut self, epoch: AttachmentEpoch) {
        self.epoch = epoch;
        self.revision = StoreRevision(self.revision.0.wrapping_add(1));
        self.invalidation_pending = false;
        self.capacity_evictions.clear();
        self.rows.clear();
    }

    pub fn install(
        &mut self,
        epoch: AttachmentEpoch,
        scope: &phoxal_cli_core::session::RobotScope,
        rows: impl IntoIterator<Item = RuntimeRow>,
    ) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        self.rows.retain(|row| &row.scope != scope);
        let rows = rows.into_iter().collect::<Vec<_>>();
        if let Some(evictions) = rows.iter().map(|row| row.capacity_evictions).max() {
            self.capacity_evictions.insert(scope.clone(), evictions);
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

    pub fn record(&mut self, epoch: AttachmentEpoch, mut row: RuntimeRow) -> Option<StoreRevision> {
        if epoch != self.epoch {
            return None;
        }
        row.capacity_evictions = self
            .capacity_evictions
            .get(&row.scope)
            .copied()
            .unwrap_or(row.capacity_evictions);
        self.rows.push_back(row);
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
    use phoxal_cli_core::session::{RobotScope, RuntimePerformanceSample};
    use phoxal_cli_observation::{ObservationQuery, QueryToken, RuntimeQuery, StoreRevision};

    use super::*;

    fn sample(scope: &RobotScope, id: &str, sequence: u64) -> RuntimeRow {
        RuntimeRow {
            scope: scope.clone(),
            sample: RuntimePerformanceSample {
                sequence,
                participant_id: id.to_string(),
                truncated: 0,
                window_ns: 1,
                step: None,
                topics: std::sync::Arc::new(Vec::new()),
                overflow: None,
            },
            capacity_evictions: sequence,
        }
    }

    #[test]
    fn installing_one_robot_snapshot_preserves_other_robot_history() {
        let epoch = AttachmentEpoch {
            supervisor_generation: 1,
            execution_id: ExecutionId::mint(),
            graph_generation: 1,
        };
        let left = RobotScope {
            namespace: "lab".to_string(),
            robot_id: "left".to_string(),
        };
        let right = RobotScope {
            namespace: "lab".to_string(),
            robot_id: "right".to_string(),
        };
        let mut store = RuntimeStore::new(epoch);
        assert!(
            store
                .install(epoch, &left, [sample(&left, "drive", 1)])
                .is_some()
        );
        assert!(
            store
                .install(epoch, &right, [sample(&right, "drive", 2)])
                .is_none()
        );
        let window = store.read(ObservationQuery {
            epoch,
            observed_revision: StoreRevision(0),
            token: QueryToken(1),
            body: RuntimeQuery {
                participant: None,
                direction: WindowDirection::Backward,
                limit: usize::MAX,
            },
        });
        assert_eq!(window.rows.len(), 2);
        assert!(window.rows.iter().any(|row| row.scope == left));
        assert!(window.rows.iter().any(|row| row.scope == right));
    }
}
