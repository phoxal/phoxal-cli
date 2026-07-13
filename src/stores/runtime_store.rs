//! `RuntimeStore`: the sidecar for session-only participant metadata the
//! persisted, JSON-stable `BoardSnapshot`/`ParticipantStatus` deliberately do
//! not carry (finding A5, target design part 5/7).
//!
//! `stores::mod`'s docs used to claim this store "already exists as
//! `supervisor::BoardBackend`/`BoardSnapshot`" - that was wrong (finding A5):
//! the board is the persisted, restart-surviving lifecycle record; artifact
//! references, declared input/output contracts, and launch ownership are
//! resolved once at launch time from the [`crate::launch_plan::LaunchPlan`]
//! and its contract-check outcome, and time-to-ready is an observation made
//! DURING the session, not a fact the board's own JSON shape should grow a
//! field for. Adding any of this to `BoardSnapshot` would leak session-only
//! data into a contract other tooling (`--message-format json`, the state
//! file `--watch` reads back) depends on staying stable.
//!
//! This store is therefore built ONCE per session, right after the launch
//! plan and contract-check outcome are known (see
//! [`RuntimeStore::from_launch_plan`]), and fed a fresh [`BoardSnapshot`] on
//! every redraw ([`RuntimeStore::observe_board`]) purely to notice each
//! participant's first observed `Ready` transition for time-to-ready - it
//! never reads back from the board for anything [`from_launch_plan`] already
//! established.
//!
//! [`from_launch_plan`]: RuntimeStore::from_launch_plan

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use phoxal::check::ParticipantContractSurface;

use crate::launch_plan::{LaunchOwnership, LaunchPlan, ParticipantExecution};
use crate::supervisor::{BoardSnapshot, ParticipantState, RouterOwnership};

/// The ownership wording the runtime UI needs. This deliberately remains a
/// session-only type: an externally reused router is neither CLI-managed nor
/// simulation-managed, but adding that case to persisted `LaunchOwnership`
/// would widen the JSON-stable launch-plan contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeOwnership {
    #[default]
    CliManaged,
    SimulationManaged,
    External,
}

impl From<LaunchOwnership> for RuntimeOwnership {
    fn from(ownership: LaunchOwnership) -> Self {
        match ownership {
            LaunchOwnership::CliManaged => Self::CliManaged,
            LaunchOwnership::SimulationManaged => Self::SimulationManaged,
        }
    }
}

/// One participant's launch-time metadata: everything [`RuntimeStore`] knows
/// about it that is not part of the board's own lifecycle record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeParticipantMetadata {
    /// The resolved artifact reference (`phoxal/service-drive@0.4.0`) for an
    /// official/catalog participant, or a human-readable local-source
    /// description for a path/git-overridden one. `None` only for a
    /// participant this store was never told about (should not happen for
    /// anything [`RuntimeStore::from_launch_plan`] actually launched).
    pub artifact_ref: Option<String>,
    pub ownership: RuntimeOwnership,
    /// Declared input contracts (`subscribe`/`ask` roles), each rendered as
    /// its generation-qualified `"<generation>::<contract>"` name, sorted and
    /// deduplicated.
    pub input_contracts: Vec<String>,
    /// Declared output contracts (`publish`/`serve` roles), same shape as
    /// [`Self::input_contracts`].
    pub output_contracts: Vec<String>,
}

impl RuntimeParticipantMetadata {
    #[must_use]
    fn ownership(ownership: impl Into<RuntimeOwnership>) -> Self {
        Self {
            ownership: ownership.into(),
            ..Self::default()
        }
    }
}

/// The in-memory sidecar for session-only participant metadata (finding A5).
/// See the module docs for why this is separate from `BoardBackend`/
/// `BoardSnapshot`.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeStore {
    session_started_at: Instant,
    metadata: BTreeMap<String, RuntimeParticipantMetadata>,
    ready_at: BTreeMap<String, Instant>,
}

impl Default for RuntimeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            session_started_at: Instant::now(),
            metadata: BTreeMap::new(),
            ready_at: BTreeMap::new(),
        }
    }

    /// Build a fully-populated store from the resolved [`LaunchPlan`] and its
    /// contract-check outcome's `contract_surfaces` - called once, right
    /// after the plan and board are built, before supervision starts. Every
    /// site tool and robot participant the plan names gets an entry;
    /// `contract_surfaces` fills in each entry's declared input/output
    /// contracts by `participant_id` (the same id the board and
    /// `ParticipantSpec` use - see `commands::check::contract_surface`'s own
    /// docs on how that id is chosen).
    #[must_use]
    pub fn from_launch_plan(
        plan: &LaunchPlan,
        contract_surfaces: &[ParticipantContractSurface],
    ) -> Self {
        let mut metadata = BTreeMap::new();
        for site in &plan.site {
            metadata.insert(
                site.id.clone(),
                RuntimeParticipantMetadata {
                    artifact_ref: Some(site.artifact_ref.clone()),
                    ..RuntimeParticipantMetadata::ownership(LaunchOwnership::CliManaged)
                },
            );
        }
        for robot in &plan.robots {
            for participant in &robot.participants {
                metadata.insert(
                    participant.launch.participant_id.clone(),
                    RuntimeParticipantMetadata {
                        artifact_ref: artifact_ref_for_execution(&participant.execution),
                        ..RuntimeParticipantMetadata::ownership(participant.launch_ownership)
                    },
                );
            }
        }
        for surface in contract_surfaces {
            let Some(entry) = metadata.get_mut(&surface.participant_id) else {
                continue;
            };
            for contract in &surface.contracts {
                let label = format!("{}::{}", contract.generation, contract.contract);
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
        Self {
            session_started_at: Instant::now(),
            metadata,
            ready_at: BTreeMap::new(),
        }
    }

    /// This participant's launch-time metadata, if this store was ever told
    /// about it (see [`Self::from_launch_plan`]).
    #[must_use]
    pub fn metadata(&self, id: &str) -> Option<&RuntimeParticipantMetadata> {
        self.metadata.get(id)
    }

    /// Apply the router ownership decision made from the live transport probe.
    /// Site launches are otherwise CLI-managed by default, but a reachable
    /// pre-existing router must render as external rather than misreported as
    /// a child this session owns.
    pub fn set_router_ownership(&mut self, id: &str, ownership: RouterOwnership) {
        if let Some(metadata) = self.metadata.get_mut(id) {
            metadata.ownership = match ownership {
                RouterOwnership::External => RuntimeOwnership::External,
                RouterOwnership::Managed => RuntimeOwnership::CliManaged,
            };
        }
    }

    /// Feed a fresh board snapshot: records the first time each participant
    /// is OBSERVED `Ready`, so [`Self::time_to_ready`] reflects a real
    /// observation rather than a fabricated one. Idempotent - a participant
    /// already marked ready keeps its original timestamp even if called
    /// again on a later, still-`Ready` snapshot.
    pub fn observe_board(&mut self, board: &BoardSnapshot) {
        let now = Instant::now();
        for status in board.participants.values() {
            if status.state == ParticipantState::Ready {
                self.ready_at.entry(status.id.clone()).or_insert(now);
            }
        }
    }

    /// How long after session start this participant was first observed
    /// `Ready` - `None` before that has happened.
    #[must_use]
    pub fn time_to_ready(&self, id: &str) -> Option<Duration> {
        self.ready_at
            .get(id)
            .map(|ready| ready.saturating_duration_since(self.session_started_at))
    }

    /// A best-effort, LOCALLY-KNOWN count of participants that declared
    /// `topic` among their input contracts (finding A5's Traffic "potential
    /// consumers" - target design part 7).
    ///
    /// This is an APPROXIMATION, not a live subscription registry: a
    /// participant's declared contract is recorded as its bare
    /// `"<generation>::<contract>"` name (see [`Self::from_launch_plan`]),
    /// while `topic` is the router's own composed wire key, which may embed
    /// additional path segments (namespace/robot/participant) around that
    /// same name rather than equal it byte-for-byte. A participant counts as
    /// a potential consumer when its declared contract name appears anywhere
    /// within the wire topic - this can undercount (a contract name that
    /// happens not to appear verbatim in the composed key) but should never
    /// overcount from unrelated topics, since contract names are already
    /// namespaced by generation.
    #[must_use]
    pub fn potential_consumers(&self, topic: &str) -> usize {
        self.metadata
            .values()
            .filter(|entry| {
                entry
                    .input_contracts
                    .iter()
                    .any(|contract| topic.contains(contract.as_str()))
            })
            .count()
    }
}

fn artifact_ref_for_execution(execution: &ParticipantExecution) -> Option<String> {
    match execution {
        ParticipantExecution::OfficialArtifact { artifact_ref } => Some(artifact_ref.clone()),
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

#[cfg(test)]
mod tests {
    use phoxal::participant::launch::{
        BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
    };
    use phoxal::participant::metadata::ParticipantMetaContract;

    use super::*;
    use crate::launch_plan::{LaunchMode, ParticipantLaunchRecord, RobotLaunch, SiteLaunch};

    fn plan_with_one_of_each() -> LaunchPlan {
        LaunchPlan {
            mode: LaunchMode::Run,
            site: vec![SiteLaunch {
                id: "tool-router".to_string(),
                artifact_ref: "phoxal/tool-router@0.1.8".to_string(),
                phoxal_config: serde_json::Value::Null,
            }],
            robots: vec![RobotLaunch {
                id: "rover-01".to_string(),
                namespace: "dev".to_string(),
                participants: vec![ParticipantLaunchRecord {
                    artifact_id: "drive".to_string(),
                    execution: ParticipantExecution::OfficialArtifact {
                        artifact_ref: "phoxal/service-drive@0.4.0".to_string(),
                    },
                    launch: ParticipantLaunch {
                        participant_id: "drive".to_string(),
                        namespace: "dev".to_string(),
                        robot_id: "rover-01".to_string(),
                        bus: BusProfile {
                            connect_endpoints: vec!["tcp/localhost:7447".to_string()],
                        },
                        clock: ClockMode::Real,
                        config: None,
                        robot_root: None,
                        component_instance: None,
                        shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
                    },
                    launch_ownership: LaunchOwnership::CliManaged,
                }],
                substitutions: Vec::new(),
            }],
        }
    }

    fn meta_contract(role: &str, generation: &str, contract: &str) -> ParticipantMetaContract {
        ParticipantMetaContract {
            role: role.to_string(),
            generation: generation.to_string(),
            contract: contract.to_string(),
            external: false,
        }
    }

    /// Finding A5: a site tool and a robot participant both get real launch
    /// metadata (artifact ref + ownership) - never the old `UNKNOWN_FIELD`
    /// placeholder - and their declared contracts split correctly by role
    /// into input (subscribe/ask) vs output (publish/serve).
    #[test]
    fn from_launch_plan_populates_artifact_ref_ownership_and_contracts() {
        let plan = plan_with_one_of_each();
        let surfaces = vec![ParticipantContractSurface {
            participant_id: "drive".to_string(),
            contracts: vec![
                meta_contract("publish", "y2026_1", "drive::State"),
                meta_contract("subscribe", "y2026_1", "drive::Target"),
            ],
        }];
        let store = RuntimeStore::from_launch_plan(&plan, &surfaces);

        let router = store
            .metadata("tool-router")
            .expect("router must be registered");
        assert_eq!(
            router.artifact_ref.as_deref(),
            Some("phoxal/tool-router@0.1.8")
        );
        assert_eq!(router.ownership, RuntimeOwnership::CliManaged);

        let drive = store.metadata("drive").expect("drive must be registered");
        assert_eq!(
            drive.artifact_ref.as_deref(),
            Some("phoxal/service-drive@0.4.0")
        );
        assert_eq!(
            drive.output_contracts,
            vec!["y2026_1::drive::State".to_string()]
        );
        assert_eq!(
            drive.input_contracts,
            vec!["y2026_1::drive::Target".to_string()]
        );
    }

    /// A participant this store was never told about (should not happen in
    /// practice) must return `None`, not panic or fabricate a value.
    #[test]
    fn metadata_is_none_for_an_unknown_participant() {
        let store = RuntimeStore::from_launch_plan(&plan_with_one_of_each(), &[]);
        assert!(store.metadata("nonexistent").is_none());
    }

    /// Finding A5's time-to-ready: `observe_board` must record the first
    /// `Ready` observation, and stay stable (not creep forward) on a later
    /// call even though the board still reports `Ready`.
    #[test]
    fn observe_board_records_first_ready_and_stays_stable() {
        let mut store = RuntimeStore::new();
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            "drive".to_string(),
            crate::supervisor::ParticipantStatus::new(
                "drive",
                crate::participant_kind::ParticipantKind::Service,
                ParticipantState::Ready,
            ),
        );

        assert!(store.time_to_ready("drive").is_none());
        store.observe_board(&board);
        let first = store
            .time_to_ready("drive")
            .expect("must be observed ready");

        std::thread::sleep(Duration::from_millis(5));
        store.observe_board(&board);
        let second = store.time_to_ready("drive").expect("still ready");
        assert_eq!(
            first, second,
            "a later observation must not move the timestamp"
        );
    }

    /// A participant never observed `Ready` (still `Starting`) must report no
    /// time-to-ready at all.
    #[test]
    fn observe_board_ignores_a_participant_that_is_not_ready_yet() {
        let mut store = RuntimeStore::new();
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            "drive".to_string(),
            crate::supervisor::ParticipantStatus::new(
                "drive",
                crate::participant_kind::ParticipantKind::Service,
                ParticipantState::Starting,
            ),
        );
        store.observe_board(&board);
        assert!(store.time_to_ready("drive").is_none());
    }

    /// Finding A5's Traffic "potential consumers": a declared input contract
    /// that appears within the router's composed wire topic counts this
    /// participant as a potential consumer; an unrelated topic does not.
    #[test]
    fn potential_consumers_counts_participants_whose_input_contract_appears_in_the_topic() {
        let plan = plan_with_one_of_each();
        let surfaces = vec![ParticipantContractSurface {
            participant_id: "drive".to_string(),
            contracts: vec![meta_contract("subscribe", "y2026_1", "drive::Target")],
        }];
        let store = RuntimeStore::from_launch_plan(&plan, &surfaces);

        assert_eq!(
            store.potential_consumers("dev/rover-01/y2026_1::drive::Target"),
            1
        );
        assert_eq!(
            store.potential_consumers("dev/rover-01/y2026_1::battery::State"),
            0
        );
    }

    #[test]
    fn external_router_ownership_overrides_the_site_default() {
        let mut store = RuntimeStore::from_launch_plan(&plan_with_one_of_each(), &[]);
        store.set_router_ownership("tool-router", RouterOwnership::External);
        assert_eq!(
            store
                .metadata("tool-router")
                .map(|metadata| metadata.ownership),
            Some(RuntimeOwnership::External)
        );
    }
}
