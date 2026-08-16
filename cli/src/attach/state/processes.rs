//! The expected-runtime table, merged with this client's own child facts.

use phoxal_cli_observation::{LocalRuntimes, ProcessObservation, ProcessTable};
use phoxal_client::supervisor::execution::Snapshot;

#[derive(Default)]
pub(crate) struct ProcessStore {
    table: ProcessTable,
}

impl ProcessStore {
    /// Replace the table from one authoritative snapshot, carrying forward the
    /// moment this client first saw each runtime present and merging in what it
    /// knows about the children it started itself.
    ///
    /// The snapshot is the whole remote truth and it is thin on purpose: a
    /// runtime is present or it is not. Everything that explains an absence -
    /// an exit status, a log path - comes from `local`, and only a session that
    /// launched the runtime has any.
    pub fn replace(&mut self, snapshot: &Snapshot, local: &LocalRuntimes) -> ProcessTable {
        let previous = std::mem::take(&mut self.table);
        let now = std::time::Instant::now();
        self.table = snapshot
            .processes
            .iter()
            .map(|row| {
                let observed_present_at = previous
                    .get(&row.participant)
                    .and_then(|old| old.observed_present_at)
                    .or_else(|| row.producer.is_some().then_some(now));
                (
                    row.participant.clone(),
                    ProcessObservation {
                        row: row.clone(),
                        observed_present_at,
                        local: local.get(&row.participant).cloned(),
                    },
                )
            })
            .collect();
        self.table.clone()
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_observation::{LocalRuntime, LocalRuntimeState};
    use phoxal_client::supervisor::execution::{Lifecycle, Process, ProcessState};
    use phoxal_runtime_contract::identity::{ParticipantId, ProducerId};
    use phoxal_runtime_contract::metadata::ParticipantKind;

    use super::*;

    fn snapshot(revision: u64, present: bool) -> Snapshot {
        Snapshot {
            revision,
            lifecycle: Lifecycle::Ready,
            startup: Vec::new(),
            processes: vec![Process {
                participant: ParticipantId::new("brain").expect("fixture participant"),
                kind: ParticipantKind::Brain,
                state: if present {
                    ProcessState::Present
                } else {
                    ProcessState::Absent
                },
                producer: present
                    .then(|| ProducerId::try_from((1_u128 << 124) | 43).expect("fixture producer")),
            }],
        }
    }

    fn brain() -> ParticipantId {
        ParticipantId::new("brain").expect("fixture participant")
    }

    /// The moment a runtime was first seen is a fact about this attachment, so
    /// it survives every later snapshot - including one where the runtime has
    /// since gone away.
    #[test]
    fn the_first_time_a_runtime_was_seen_present_survives_later_snapshots() {
        let mut store = ProcessStore::default();
        let absent = store.replace(&snapshot(1, false), &LocalRuntimes::new());
        assert_eq!(absent[&brain()].observed_present_at, None);
        assert!(!absent[&brain()].present());

        let present = store.replace(&snapshot(2, true), &LocalRuntimes::new());
        let first = present[&brain()]
            .observed_present_at
            .expect("a present row records when it was first seen");
        assert!(present[&brain()].present());

        let gone = store.replace(&snapshot(3, false), &LocalRuntimes::new());
        assert_eq!(gone[&brain()].observed_present_at, Some(first));
    }

    /// A session that launched the runtime carries the answer the snapshot
    /// cannot give: why an absent row is absent, and where to read the rest.
    #[test]
    fn a_launched_runtimes_own_exit_is_merged_onto_its_absent_row() {
        let mut store = ProcessStore::default();
        let local = LocalRuntimes::from([(
            brain(),
            LocalRuntime {
                state: LocalRuntimeState::Exited {
                    status: "exit status: 1".to_string(),
                },
                log: std::path::PathBuf::from("/tmp/rover/.phoxal/run/log/brain.log"),
            },
        )]);

        let table = store.replace(&snapshot(1, false), &local);
        let observed = table[&brain()]
            .local
            .as_ref()
            .expect("a launched runtime carries its own child facts");
        assert_eq!(observed.state.label(), "exit status: 1");
        assert_eq!(
            observed.log,
            std::path::PathBuf::from("/tmp/rover/.phoxal/run/log/brain.log")
        );

        // An attachment to somebody else's execution has none of this, and the
        // row is still a complete row.
        let attached = store.replace(&snapshot(2, false), &LocalRuntimes::new());
        assert!(attached[&brain()].local.is_none());
    }
}
