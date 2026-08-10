//! The supervised process table, with this client's own local timings.

use phoxal_api::supervisor::snapshot::{ProcessState, Snapshot};
use phoxal_cli_observation::{ProcessObservation, ProcessTable};

#[derive(Default)]
pub(crate) struct ProcessStore {
    table: ProcessTable,
}

impl ProcessStore {
    /// Replace the table from one authoritative snapshot, carrying forward the
    /// local timings a row has earned.
    ///
    /// A restart is detected from the daemon's own restart counter, so a
    /// respawned process starts its clock again rather than appearing to have
    /// been up since the execution began.
    pub fn replace(&mut self, snapshot: &Snapshot) -> ProcessTable {
        let previous = std::mem::take(&mut self.table);
        let now = std::time::Instant::now();
        self.table = snapshot
            .processes
            .iter()
            .map(|row| {
                let old = previous.get(&row.participant);
                let restarted = old.is_some_and(|old| {
                    row.restarts > old.row.restarts
                        || (row.state == ProcessState::Restarting
                            && old.row.state != ProcessState::Restarting)
                });
                let observed_started_at = if restarted {
                    now
                } else {
                    old.map_or(now, |old| old.observed_started_at)
                };
                let observed_first_ready_at = if restarted {
                    None
                } else {
                    old.and_then(|old| old.observed_first_ready_at)
                }
                .or((row.state == ProcessState::Ready).then_some(now));
                let observed_ended_at =
                    if matches!(row.state, ProcessState::Failed | ProcessState::Stopped) {
                        old.and_then(|old| old.observed_ended_at).or(Some(now))
                    } else {
                        None
                    };
                (
                    row.participant.clone(),
                    ProcessObservation {
                        row: row.clone(),
                        observed_started_at,
                        observed_ended_at,
                        observed_first_ready_at,
                    },
                )
            })
            .collect();
        self.table.clone()
    }
}

#[cfg(test)]
mod tests {
    use phoxal_api::supervisor::snapshot::{DesiredState, Lifecycle, Process};
    use phoxal_runtime_contract::identity::{ParticipantId, ProducerId};
    use phoxal_runtime_contract::metadata::ParticipantKind;

    use super::*;

    fn snapshot(revision: u64, state: ProcessState, restarts: u64) -> Snapshot {
        Snapshot {
            revision,
            lifecycle: Lifecycle::Ready,
            startup: Vec::new(),
            processes: vec![Process {
                participant: ParticipantId::new("brain").expect("fixture participant"),
                kind: ParticipantKind::Service,
                component: None,
                desired: DesiredState::Running,
                state,
                pid: Some(42),
                producer: Some(ProducerId::try_from((1_u128 << 124) | 43).unwrap()),
                restarts,
                failure: None,
            }],
            failure: None,
        }
    }

    #[test]
    fn a_restart_resets_the_local_timings_a_steady_row_keeps() {
        let mut store = ProcessStore::default();
        let table = store.replace(&snapshot(1, ProcessState::Ready, 0));
        let brain = ParticipantId::new("brain").expect("fixture participant");
        let first_ready = table[&brain].observed_first_ready_at;
        assert!(first_ready.is_some());
        assert!(table[&brain].present());

        let steady = store.replace(&snapshot(2, ProcessState::Ready, 0));
        assert_eq!(steady[&brain].observed_first_ready_at, first_ready);

        let restarted = store.replace(&snapshot(3, ProcessState::Starting, 1));
        assert_eq!(restarted[&brain].observed_first_ready_at, None);
        assert!(restarted[&brain].observed_started_at > table[&brain].observed_started_at);
    }
}
