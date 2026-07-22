//! Session-only runtime metadata and observations. Nothing here is persisted
//! in BoardSnapshot or exposed by the stable plain/JSON status paths.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use phoxal::check::ParticipantContractSurface;

use crate::project::launch_plan::{LaunchOwnership, LaunchPlan, ParticipantExecution};
use crate::session::board::{BoardSnapshot, ParticipantState};
use crate::session::{ProcessKey, RobotKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeOwnership {
    #[default]
    CliManaged,
    SimulationManaged,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeOrigin {
    UserService,
    #[default]
    Framework,
}

impl From<LaunchOwnership> for RuntimeOwnership {
    fn from(ownership: LaunchOwnership) -> Self {
        match ownership {
            LaunchOwnership::CliManaged => Self::CliManaged,
            LaunchOwnership::SimulationManaged => Self::SimulationManaged,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeParticipantMetadata {
    pub artifact_ref: Option<String>,
    pub ownership: RuntimeOwnership,
    pub origin: RuntimeOrigin,
    pub input_contracts: Vec<String>,
    pub output_contracts: Vec<String>,
}

impl RuntimeParticipantMetadata {
    fn ownership(ownership: impl Into<RuntimeOwnership>) -> Self {
        Self {
            ownership: ownership.into(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeObservation {
    pub started_at: Instant,
    ended_at: Option<Instant>,
    pub first_ready_at: Option<Instant>,
    pub supervisor_restart_count: u32,
    pub last_state: ParticipantState,
}

impl RuntimeObservation {
    #[must_use]
    pub fn uptime(&self, now: Instant) -> Duration {
        self.ended_at
            .unwrap_or(now)
            .saturating_duration_since(self.started_at)
    }

    #[must_use]
    pub fn displayed_restarts(&self) -> u32 {
        self.supervisor_restart_count
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeStore {
    session_started_at: Instant,
    metadata: BTreeMap<String, RuntimeParticipantMetadata>,
    observations: BTreeMap<String, RuntimeObservation>,
}

impl Default for RuntimeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeStore {
    #[must_use]
    pub fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(now: Instant) -> Self {
        Self {
            session_started_at: now,
            metadata: BTreeMap::new(),
            observations: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn from_launch_plan(
        plan: &LaunchPlan,
        contract_surfaces: &[ParticipantContractSurface],
    ) -> Self {
        let mut store = Self::new();
        for site in &plan.site {
            store.metadata.insert(
                ProcessKey::project(&site.id).to_string(),
                RuntimeParticipantMetadata {
                    artifact_ref: Some(site.artifact_ref.clone()),
                    ..RuntimeParticipantMetadata::ownership(LaunchOwnership::CliManaged)
                },
            );
        }
        for robot in &plan.robots {
            for participant in &robot.participants {
                store.metadata.insert(
                    ProcessKey::robot(
                        RobotKey::new(&robot.namespace, &robot.id),
                        &participant.launch.participant_id,
                    )
                    .to_string(),
                    RuntimeParticipantMetadata {
                        artifact_ref: artifact_ref_for_execution(&participant.execution),
                        origin: origin_for_execution(&participant.execution),
                        ..RuntimeParticipantMetadata::ownership(participant.launch_ownership)
                    },
                );
            }
        }
        for surface in contract_surfaces {
            for (key, entry) in &mut store.metadata {
                let process_key = key
                    .parse::<ProcessKey>()
                    .expect("store inserted valid process key");
                if process_key.id != surface.participant_id {
                    continue;
                }
                for contract in &surface.contracts {
                    let label = format!("{}::{}", contract.version, contract.contract);
                    match contract.role.as_str() {
                        "publish" | "serve" => entry.output_contracts.push(label),
                        "subscribe" | "ask" => entry.input_contracts.push(label),
                        _ => {}
                    }
                }
                entry.input_contracts.sort();
                entry.input_contracts.dedup();
                entry.output_contracts.sort();
                entry.output_contracts.dedup();
            }
        }
        store
    }

    #[must_use]
    pub fn metadata(&self, id: &str) -> Option<&RuntimeParticipantMetadata> {
        self.metadata.get(id).or_else(|| {
            let mut matches = self.metadata.iter().filter(|(key, _)| {
                key.parse::<ProcessKey>()
                    .is_ok_and(|process_key| process_key.id == id)
            });
            let (_, value) = matches.next()?;
            matches.next().is_none().then_some(value)
        })
    }

    #[must_use]
    pub fn observation(&self, id: &str) -> Option<&RuntimeObservation> {
        self.observations.get(id)
    }

    #[must_use]
    pub fn session_uptime(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.session_started_at)
    }

    #[doc(hidden)]
    pub fn set_test_ownership(&mut self, id: &str, ownership: RuntimeOwnership) {
        self.metadata.entry(id.to_string()).or_default().ownership = ownership;
    }

    #[doc(hidden)]
    pub fn set_test_origin(&mut self, id: &str, origin: RuntimeOrigin) {
        self.metadata.entry(id.to_string()).or_default().origin = origin;
    }

    #[doc(hidden)]
    pub fn set_test_contracts(&mut self, id: &str, inputs: Vec<String>, outputs: Vec<String>) {
        let metadata = self.metadata.entry(id.to_string()).or_default();
        metadata.input_contracts = inputs;
        metadata.output_contracts = outputs;
    }

    pub fn observe_board(&mut self, board: &BoardSnapshot) {
        self.observe_board_at(board, Instant::now());
    }

    pub(crate) fn observe_board_at(&mut self, board: &BoardSnapshot, now: Instant) {
        self.observations
            .retain(|id, _| board.participants.contains_key(id));
        for status in board.participants.values() {
            let observation =
                self.observations
                    .entry(status.id.clone())
                    .or_insert(RuntimeObservation {
                        started_at: now,
                        ended_at: None,
                        first_ready_at: None,
                        supervisor_restart_count: status.restart_count,
                        last_state: status.state,
                    });

            let supervised_restart = status.restart_count > observation.supervisor_restart_count
                || (status.state == ParticipantState::Restarting
                    && observation.last_state != ParticipantState::Restarting);
            if supervised_restart {
                observation.started_at = now;
                observation.ended_at = None;
                observation.first_ready_at = None;
            }
            observation.supervisor_restart_count = status.restart_count;
            if status.state == ParticipantState::Ready {
                observation.first_ready_at.get_or_insert(now);
            }
            if matches!(
                status.state,
                ParticipantState::Failed | ParticipantState::Stopped
            ) {
                observation.ended_at.get_or_insert(now);
            } else {
                observation.ended_at = None;
            }
            observation.last_state = status.state;
        }
    }

    #[must_use]
    pub fn time_to_ready(&self, id: &str) -> Option<Duration> {
        let observation = self.observation(id)?;
        observation
            .first_ready_at
            .map(|ready| ready.saturating_duration_since(observation.started_at))
    }
}

fn artifact_ref_for_execution(execution: &ParticipantExecution) -> Option<String> {
    match execution {
        ParticipantExecution::OfficialArtifact { artifact_ref }
        | ParticipantExecution::OfficialTool { artifact_ref } => Some(artifact_ref.clone()),
        ParticipantExecution::UserService { crate_dir } => {
            Some(format!("local user service: {}", crate_dir.display()))
        }
        ParticipantExecution::SourceArtifact { kind, crate_dir } => {
            Some(format!("local {kind} source: {}", crate_dir.display()))
        }
        ParticipantExecution::ComponentDriver { crate_dir } => {
            Some(format!("local component driver: {}", crate_dir.display()))
        }
    }
}

fn origin_for_execution(execution: &ParticipantExecution) -> RuntimeOrigin {
    match execution {
        ParticipantExecution::UserService { .. } => RuntimeOrigin::UserService,
        ParticipantExecution::OfficialArtifact { .. }
        | ParticipantExecution::OfficialTool { .. }
        | ParticipantExecution::SourceArtifact { .. }
        | ParticipantExecution::ComponentDriver { .. } => RuntimeOrigin::Framework,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ParticipantKind;
    use crate::session::board::ParticipantStatus;

    fn board(state: ParticipantState, restarts: u32) -> BoardSnapshot {
        let mut board = BoardSnapshot::default();
        let mut status = ParticipantStatus::new("drive", ParticipantKind::Service, state);
        status.restart_count = restarts;
        board.participants.insert("drive".to_string(), status);
        board
    }

    #[test]
    fn supervised_restart_resets_process_uptime() {
        let start = Instant::now();
        let mut store = RuntimeStore::new_at(start);
        store.observe_board_at(&board(ParticipantState::Ready, 0), start);
        let restarted = start + Duration::from_secs(8);
        store.observe_board_at(&board(ParticipantState::Restarting, 1), restarted);
        let observation = store.observation("drive").unwrap();
        assert_eq!(observation.uptime(restarted), Duration::ZERO);
        assert_eq!(observation.displayed_restarts(), 1);
        assert!(store.time_to_ready("drive").is_none());
    }

    #[test]
    fn terminal_runtime_state_freezes_uptime_until_a_restart() {
        let start = Instant::now();
        let mut store = RuntimeStore::new_at(start);
        store.observe_board_at(&board(ParticipantState::Ready, 0), start);
        let failed_at = start + Duration::from_secs(4);
        store.observe_board_at(&board(ParticipantState::Failed, 0), failed_at);
        assert_eq!(
            store
                .observation("drive")
                .expect("failed runtime observation")
                .uptime(start + Duration::from_secs(40)),
            Duration::from_secs(4)
        );

        let restarted_at = start + Duration::from_secs(41);
        store.observe_board_at(&board(ParticipantState::Restarting, 1), restarted_at);
        assert_eq!(
            store
                .observation("drive")
                .expect("restarted runtime observation")
                .uptime(restarted_at),
            Duration::ZERO
        );
    }

    #[test]
    fn observations_are_pruned_when_participants_leave_the_board() {
        let start = Instant::now();
        let mut store = RuntimeStore::new_at(start);
        store.observe_board_at(&board(ParticipantState::Ready, 0), start);
        assert!(store.observation("drive").is_some());

        store.observe_board_at(&BoardSnapshot::default(), start);
        assert!(store.observation("drive").is_none());

        let returned = start + Duration::from_secs(9);
        store.observe_board_at(&board(ParticipantState::Ready, 0), returned);
        assert_eq!(store.observation("drive").unwrap().started_at, returned);
    }

    #[test]
    fn duplicate_participant_ids_require_scoped_runtime_lookup() {
        let mut store = RuntimeStore::new();
        let left = ProcessKey::robot(RobotKey::new("lab", "alpha"), "motion").to_string();
        let right = ProcessKey::robot(RobotKey::new("lab", "beta"), "motion").to_string();
        store
            .metadata
            .insert(left.clone(), RuntimeParticipantMetadata::default());
        store
            .metadata
            .insert(right.clone(), RuntimeParticipantMetadata::default());

        assert!(store.metadata(&left).is_some());
        assert!(store.metadata(&right).is_some());
        assert!(
            store.metadata("motion").is_none(),
            "bare lookup is ambiguous"
        );
    }
}
