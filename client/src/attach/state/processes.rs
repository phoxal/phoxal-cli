use phoxal_cli_core::runtime::{ParticipantState, ProcessScope, ProcessState, RobotKey};
use phoxal_cli_observation::{ProcessObservation, ProcessTable};
use phoxal_cli_protocol::SupervisorSnapshotV0;

#[derive(Default)]
pub(crate) struct ProcessStore {
    table: ProcessTable,
}

impl ProcessStore {
    pub fn replace(&mut self, snapshot: &SupervisorSnapshotV0) -> ProcessTable {
        let previous = std::mem::take(&mut self.table);
        let now = std::time::Instant::now();
        self.table = snapshot
            .processes
            .iter()
            .map(|(key, entry)| {
                let old = previous.get(key);
                let state = participant_state(entry.status.actual);
                let restarted = old.is_some_and(|old| {
                    entry.status.restart_count_in_generation
                        > old.entry.status.restart_count_in_generation
                        || (state == ParticipantState::Restarting
                            && old.state != ParticipantState::Restarting)
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
                .or((state == ParticipantState::Ready).then_some(now));
                let ended_at =
                    if matches!(state, ParticipantState::Failed | ParticipantState::Stopped) {
                        old.and_then(|old| old.ended_at).or(Some(now))
                    } else {
                        None
                    };
                (
                    key.clone(),
                    ProcessObservation {
                        key: key.clone(),
                        entry: entry.clone(),
                        kind: entry.descriptor.kind,
                        state,
                        present: old.and_then(|old| old.present),
                        robot: match &key.scope {
                            ProcessScope::Robot(robot) => Some(robot.clone()),
                            ProcessScope::Project => None,
                        },
                        started_at,
                        ended_at,
                        first_ready_at,
                        user_service: entry.descriptor.owner == "project",
                    },
                )
            })
            .collect();
        self.table.clone()
    }

    pub fn clear_graph(&mut self) {
        self.table.clear();
    }

    pub fn record_presence(
        &mut self,
        robot: &RobotKey,
        participant: &str,
        present: bool,
    ) -> Option<ProcessTable> {
        let process = self.table.values_mut().find(|process| {
            process.robot.as_ref() == Some(robot) && process.key.id == participant
        })?;
        if process.present == Some(present) {
            return None;
        }
        process.present = Some(present);
        Some(self.table.clone())
    }
}

fn participant_state(state: ProcessState) -> ParticipantState {
    match state {
        ProcessState::Starting => ParticipantState::Starting,
        ProcessState::Ready => ParticipantState::Ready,
        ProcessState::Degraded => ParticipantState::Degraded,
        ProcessState::Restarting => ParticipantState::Restarting,
        ProcessState::Failed => ParticipantState::Failed,
        ProcessState::Stopped => ParticipantState::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::runtime::{
        ParticipantKind, ProcessDescriptor, ProcessEntry, ProcessKey, ProcessStatus,
        StartupRequirement,
    };

    use super::*;

    #[test]
    fn supervisor_refresh_preserves_independent_liveliness_presence() {
        let robot = RobotKey::new("lab", "rover");
        let key = ProcessKey::robot(robot.clone(), "drive");
        let mut snapshot = SupervisorSnapshotV0::default();
        snapshot.processes.insert(
            key.clone(),
            ProcessEntry {
                descriptor: ProcessDescriptor {
                    key: key.clone(),
                    kind: ParticipantKind::Service,
                    artifact: "drive".to_string(),
                    owner: "project".to_string(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus {
                    actual: ProcessState::Ready,
                    ..ProcessStatus::default()
                },
            },
        );

        let mut store = ProcessStore::default();
        store.replace(&snapshot);
        assert!(store.record_presence(&robot, "drive", true).is_some());
        snapshot.revision += 1;
        let refreshed = store.replace(&snapshot);
        assert_eq!(refreshed[&key].present, Some(true));
    }
}
