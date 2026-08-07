//! The supervised process table, with this client's own local timings.

use phoxal_cli_observation::{ProcessObservation, ProcessTable};
use phoxal_supervisor_api::{ProcessState, Snapshot};

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
                let old = previous.get(&row.key);
                let restarted = old.is_some_and(|old| {
                    row.restarts > old.row.restarts
                        || (row.state == ProcessState::Restarting
                            && old.state != ProcessState::Restarting)
                });
                let started_at = if restarted {
                    now
                } else {
                    old.map_or(now, |old| old.started_at)
                };
                let first_ready_at = if restarted {
                    None
                } else {
                    old.and_then(|old| old.first_ready_at)
                }
                .or((row.state == ProcessState::Ready).then_some(now));
                let ended_at = if matches!(row.state, ProcessState::Failed | ProcessState::Stopped)
                {
                    old.and_then(|old| old.ended_at).or(Some(now))
                } else {
                    None
                };
                (
                    row.key.clone(),
                    ProcessObservation {
                        key: row.key.clone(),
                        row: row.clone(),
                        state: row.state,
                        started_at,
                        ended_at,
                        first_ready_at,
                    },
                )
            })
            .collect();
        self.table.clone()
    }
}

#[cfg(test)]
mod tests {
    use phoxal_runtime_contract::ProducerId;
    use phoxal_supervisor_api::{
        DesiredState, ExecutionMode, Lifecycle, Name, Process, ProcessKey, RobotIdentity,
        StartupRequirement,
    };

    use super::*;

    fn snapshot(revision: u64, state: ProcessState, restarts: u64) -> Snapshot {
        Snapshot {
            revision,
            robot: RobotIdentity {
                id: Name::new("rover"),
                namespace: Name::new("lab"),
            },
            mode: ExecutionMode::Real,
            lifecycle: Lifecycle::Ready,
            startup: Vec::new(),
            processes: vec![Process {
                key: ProcessKey::Brain,
                component: None,
                startup: StartupRequirement::Required,
                desired: DesiredState::Running,
                state,
                pid: Some(42),
                producer: Some(ProducerId::try_from(0x2b).unwrap()),
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
        let first_ready = table[&ProcessKey::Brain].first_ready_at;
        assert!(first_ready.is_some());
        assert!(table[&ProcessKey::Brain].present());

        let steady = store.replace(&snapshot(2, ProcessState::Ready, 0));
        assert_eq!(steady[&ProcessKey::Brain].first_ready_at, first_ready);

        let restarted = store.replace(&snapshot(3, ProcessState::Starting, 1));
        assert_eq!(restarted[&ProcessKey::Brain].first_ready_at, None);
        assert!(restarted[&ProcessKey::Brain].started_at > table[&ProcessKey::Brain].started_at);
    }
}
