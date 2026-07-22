use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::check::{
    CheckGraphContext, build_emit_apis_from_source, check_artifact_refs_from_resolved,
    extract_emit_apis_from_staged_runtime, extract_emit_apis_from_staged_tool,
    fetch_emit_apis_from_tool, run_check_with_context, source_participants_from_resolved,
    tool_participants_from_resolved,
};
use crate::component_driver::component_driver_crate_dir;
use crate::resolver::resolve;
use crate::run::{DriverPolicy, RunOptions, source_spec_from_launch_record};
use crate::simulation::{
    SimulateMode, SimulateOptions, build_checked_sim_launch_plan, resolve_project,
};
use crate::supervisor::{BoardBackend, ParticipantSpec, SupervisorAction};
use phoxal_cli_core::check::source::{SourceParticipant, SourceParticipantKind};
use phoxal_cli_core::project::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, LaunchPlan, PlanContext, build_launch_plan,
};
use phoxal_cli_core::project::resolver::{
    ResolveOptions, ResolvedRobot, discover_robot_yaml, load_robot_with_extras,
    load_robot_with_extras_and_overlays,
};
use phoxal_cli_core::project::tooling::hash_tree;
use phoxal_cli_core::session::{ParticipantKind, human};

const WATCH_POLL: Duration = Duration::from_millis(500);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(650);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchMode {
    Run,
    Sim,
}

/// The shared `ParticipantKind` (`Service`/`Driver`/`Tool`/`Simulator`) - a
/// watch target only ever needs the role split, never a local/suite bit
/// (every watch target is by construction a locally-changing source
/// directory), so this is a plain alias rather than a wrapping type.
pub(crate) type WatchTargetKind = ParticipantKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchTarget {
    pub key: String,
    pub kind: WatchTargetKind,
    pub label: String,
    pub crate_dir: PathBuf,
    pub participant_ids: Vec<String>,
    pub board_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DebounceQueue {
    delay: Duration,
    pending: BTreeMap<String, Instant>,
}

impl DebounceQueue {
    pub(crate) fn new(delay: Duration) -> Self {
        Self {
            delay,
            pending: BTreeMap::new(),
        }
    }

    pub(crate) fn note_change(&mut self, key: impl Into<String>, now: Instant) {
        self.pending.insert(key.into(), now + self.delay);
    }

    pub(crate) fn due(&mut self, now: Instant) -> Vec<String> {
        let due = self
            .pending
            .iter()
            .filter_map(|(key, deadline)| (*deadline <= now).then_some(key.clone()))
            .collect::<Vec<_>>();
        for key in &due {
            self.pending.remove(key);
        }
        due
    }
}

pub(crate) struct RunWatchConfig {
    pub ctx: PlanContext,
    pub options: RunOptions,
    pub live_ids: BTreeSet<String>,
    pub board: BoardBackend,
    pub action_tx: mpsc::Sender<SupervisorAction>,
}

pub(crate) struct SimWatchConfig {
    pub ctx: PlanContext,
    pub options: SimulateOptions,
    pub live_ids: BTreeSet<String>,
    pub board: BoardBackend,
    pub action_tx: mpsc::Sender<SupervisorAction>,
}

pub(crate) fn spawn_run_watch(config: RunWatchConfig) -> JoinHandle<()> {
    let mut targets = watch_targets_from_sources(
        WatchMode::Run,
        &config.ctx.resolved,
        &config.ctx.source_participants,
        &config.live_ids,
    );
    targets.push(plan_input_target(
        &config.ctx.project_root,
        &config.ctx.source_participants,
    ));
    spawn_watch_loop(
        WatchMode::Run,
        config.ctx.project_root,
        WatchOptions::Run(config.options),
        config.board,
        config.action_tx,
        targets,
    )
}

pub(crate) fn spawn_sim_watch(config: SimWatchConfig) -> JoinHandle<()> {
    let mut targets = watch_targets_from_sources(
        WatchMode::Sim,
        &config.ctx.resolved,
        &config.ctx.source_participants,
        &config.live_ids,
    );
    targets.push(plan_input_target(
        &config.ctx.project_root,
        &config.ctx.source_participants,
    ));
    spawn_watch_loop(
        WatchMode::Sim,
        config.ctx.project_root,
        WatchOptions::Sim(config.options),
        config.board,
        config.action_tx,
        targets,
    )
}

#[derive(Debug, Clone)]
enum WatchOptions {
    Run(RunOptions),
    Sim(SimulateOptions),
}

fn spawn_watch_loop(
    mode: WatchMode,
    project_root: PathBuf,
    options: WatchOptions,
    board: BoardBackend,
    action_tx: mpsc::Sender<SupervisorAction>,
    targets: Vec<WatchTarget>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if targets.is_empty() {
            return;
        }
        let mut hashes = initial_hashes(&targets, &board);
        let targets_by_key = targets
            .into_iter()
            .map(|target| (target.key.clone(), target))
            .collect::<BTreeMap<_, _>>();
        let mut debounce = DebounceQueue::new(WATCH_DEBOUNCE);
        let mut ticker = tokio::time::interval(WATCH_POLL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            for target in targets_by_key.values() {
                match hash_watch_target(target) {
                    Ok(hash) if hashes.get(&target.key) != Some(&hash) => {
                        hashes.insert(target.key.clone(), hash);
                        debounce.note_change(target.key.clone(), Instant::now());
                    }
                    Ok(_) => {}
                    Err(error) => {
                        set_note_all(
                            &board,
                            target,
                            format!("rebuilding {}... failed: {error:#}", target.label),
                        );
                    }
                }
            }

            for key in debounce.due(Instant::now()) {
                let Some(target) = targets_by_key.get(&key).cloned() else {
                    continue;
                };
                handle_target_change(
                    mode,
                    project_root.clone(),
                    options.clone(),
                    board.clone(),
                    action_tx.clone(),
                    target,
                )
                .await;
            }
        }
    })
}

fn initial_hashes(targets: &[WatchTarget], board: &BoardBackend) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    for target in targets {
        match hash_watch_target(target) {
            Ok(hash) => {
                hashes.insert(target.key.clone(), hash);
            }
            Err(error) => {
                set_note_all(
                    board,
                    target,
                    format!("watch disabled for {}: {error:#}", target.label),
                );
            }
        }
    }
    hashes
}

fn plan_input_target(project_root: &Path, sources: &[SourceParticipant]) -> WatchTarget {
    let ids = sources
        .iter()
        .map(|source| source.name.clone())
        .collect::<Vec<_>>();
    WatchTarget {
        key: "plan-inputs".to_string(),
        kind: ParticipantKind::Service,
        label: "launch plan".to_string(),
        crate_dir: project_root.to_path_buf(),
        participant_ids: ids.clone(),
        board_ids: ids,
    }
}

fn hash_watch_target(target: &WatchTarget) -> Result<String> {
    if target.key != "plan-inputs" {
        return hash_tree(&target.crate_dir);
    }
    use sha2::{Digest, Sha256};
    let mut files = std::fs::read_dir(&target.crate_dir)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && matches!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("yaml" | "yml" | "toml" | "lock" | "json")
                )
        })
        .collect::<Vec<_>>();
    files.sort();
    let mut digest = Sha256::new();
    for path in files {
        digest.update(
            path.file_name()
                .expect("root file has name")
                .as_encoded_bytes(),
        );
        digest.update(std::fs::read(path)?);
    }
    Ok(hex::encode(digest.finalize()))
}

async fn handle_target_change(
    mode: WatchMode,
    project_root: PathBuf,
    options: WatchOptions,
    board: BoardBackend,
    action_tx: mpsc::Sender<SupervisorAction>,
    target: WatchTarget,
) {
    let started = Instant::now();
    let material_root = project_root.clone();
    set_note_all(&board, &target, format!("rebuilding {}...", target.label));
    let result = match (mode, options) {
        (_, _)
            if matches!(
                target.kind,
                WatchTargetKind::Tool | WatchTargetKind::Simulator
            ) =>
        {
            Ok(WatchOutcome::RestartNeeded)
        }
        (WatchMode::Run, WatchOptions::Run(options)) => {
            let worker_target = target.clone();
            tokio::task::spawn_blocking(move || {
                recheck_run_target(&project_root, &options, &worker_target)
            })
            .await
            .unwrap_or_else(|error| Err(anyhow!("watch worker failed: {error}")))
        }
        (WatchMode::Sim, WatchOptions::Sim(options)) => {
            let worker_target = target.clone();
            tokio::task::spawn_blocking(move || {
                recheck_sim_target(&project_root, &options, &worker_target)
            })
            .await
            .unwrap_or_else(|error| Err(anyhow!("watch worker failed: {error}")))
        }
        _ => Err(anyhow!("watch mode/options mismatch")),
    };

    apply_watch_result(&material_root, &board, &action_tx, &target, started, result).await;
}

async fn apply_watch_result(
    project_root: &Path,
    board: &BoardBackend,
    action_tx: &mpsc::Sender<SupervisorAction>,
    target: &WatchTarget,
    started: Instant,
    result: Result<WatchOutcome>,
) {
    match result {
        Ok(WatchOutcome::Revision { plan, mut specs }) => {
            if specs.is_empty() {
                set_note_all(
                    board,
                    target,
                    format!(
                        "rebuilding {}... ok {}, no live instances",
                        target.label,
                        elapsed_label(started)
                    ),
                );
                return;
            }
            let note = format!(
                "rebuilding {}... ok {}, restarted",
                target.label,
                elapsed_label(started)
            );
            let revision = match phoxal_cli_core::project::launch_plan::PlanRevision::compile(
                board.supervisor_snapshot().plan_revision.saturating_add(1),
                plan,
            ) {
                Ok(revision) => revision,
                Err(error) => {
                    set_note_all(
                        board,
                        target,
                        format!("watch plan compilation failed: {error:#}"),
                    );
                    return;
                }
            };
            let new_ids = specs
                .iter()
                .map(|spec| spec.id.as_str())
                .collect::<BTreeSet<_>>();
            let remove_ids = target
                .participant_ids
                .iter()
                .filter(|id| !new_ids.contains(id.as_str()))
                .cloned()
                .collect();
            if let Err(error) =
                crate::supervisor::materialize_plan_binaries(project_root, &revision, &mut specs)
            {
                set_note_all(
                    board,
                    target,
                    format!("watch plan materialization failed: {error:#}"),
                );
                return;
            }
            if action_tx
                .send(SupervisorAction::ReconcilePlan {
                    revision,
                    specs,
                    remove_ids,
                    note,
                })
                .await
                .is_err()
            {
                set_note_all(board, target, "watch stopped: supervisor channel closed");
            }
        }
        Ok(WatchOutcome::MetadataOnly) => {
            set_note_all(
                board,
                target,
                format!(
                    "rebuilding {}... ok {}, metadata refreshed",
                    target.label,
                    elapsed_label(started)
                ),
            );
        }
        Ok(WatchOutcome::RestartNeeded) => {
            set_note_all(
                board,
                target,
                format!(
                    "rebuilding {}... ok {}, restart needed",
                    target.label,
                    elapsed_label(started)
                ),
            );
        }
        Err(error) => {
            set_note_all(
                board,
                target,
                format!("rebuilding {}... failed: {error:#}", target.label),
            );
        }
    }
}

fn elapsed_label(started: Instant) -> String {
    human::duration(started.elapsed())
}

fn set_note_all(board: &BoardBackend, target: &WatchTarget, note: impl AsRef<str>) {
    let note = note.as_ref();
    for id in &target.board_ids {
        board.set_note_by_participant_id(id, note);
    }
}

enum WatchOutcome {
    Revision {
        plan: LaunchPlan,
        specs: Vec<ParticipantSpec>,
    },
    MetadataOnly,
    RestartNeeded,
}

fn recheck_run_target(
    project_root: &Path,
    options: &RunOptions,
    target: &WatchTarget,
) -> Result<WatchOutcome> {
    let robot_path = discover_robot_yaml(project_root)
        .with_context(|| format!("failed to find robot.yaml from {}", project_root.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let loaded = if options.overlays.is_empty() {
        load_robot_with_extras(&robot_path)?
    } else {
        load_robot_with_extras_and_overlays(&robot_path, &options.overlays)?
    };
    let suite = crate::commands::load_suite_for_robot_from_source(
        options.suite_source.clone(),
        project_root,
        &loaded.extras,
    )?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        suite.as_ref(),
        ResolveOptions {
            resolve_source_commits: true,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )?;
    // Every source participant rebuilds live on every recheck - there is no
    // more disk cache to scope around (docs: `check::build_emit_apis_from_source`
    // never caches), so a single changed crate simply triggers a full rebuild
    // of the whole source graph rather than just the changed one.
    let source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;
    let platform_refs = check_artifact_refs_from_resolved(&resolved);
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::check::component_driver_runtimes_by_ref(&resolved));
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &source_participants,
        CheckGraphContext {
            manifest_extras: &loaded.extras,
        },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_runtime(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the suite"
            ))
        },
        fetch_emit_apis_from_tool,
        build_emit_apis_from_source,
    )?;
    crate::check::ensure_check_outcome_ok(&resolved.train, &outcome)?;
    let mut plan = build_launch_plan(
        LaunchMode::Run,
        &[CheckedRobotLaunchInput {
            project_root,
            resolved: &resolved,
            manifest_extras: &loaded.extras,
            checked_participants: &outcome.checked_participants,
            substitutions: &[],
            source_participants: &source_participants,
        }],
    )?;
    let driver_policy = DriverPolicy::from_options(options, &plan)?;
    let mut coherence_plan = plan.clone();
    for robot in &mut coherence_plan.robots {
        robot
            .participants
            .retain(|participant| driver_policy.launches(participant));
    }
    let coherence_graph =
        crate::check::robot_contract_surfaces(&resolved.robot.robot.id, &outcome.contract_surfaces);
    let coherence = crate::check::coherence_for_launch_plan(&coherence_plan, &[coherence_graph])?;
    crate::check::enforce_coherence(crate::check::CoherenceVerb::Run, &coherence)?;
    let mut specs = specs_for_target(&plan, target)?;
    let endpoint = crate::run::project_router_endpoint(project_root);
    crate::run::apply_session_connect(&mut plan, &mut specs, &endpoint);
    Ok(WatchOutcome::Revision { plan, specs })
}

fn recheck_sim_target(
    project_root: &Path,
    options: &SimulateOptions,
    target: &WatchTarget,
) -> Result<WatchOutcome> {
    let resolved = resolve_project(project_root, options.clone(), SimulateMode::Live)?;
    // A watch recheck only needs the plan itself (to diff specs), not the
    // contract surfaces `build_checked_sim_launch_plan` now also returns for
    // `RuntimeStore` (finding A5) - this path never builds a fresh session.
    let (mut plan, _contract_surfaces) = build_checked_sim_launch_plan(
        &resolved.project_root,
        &resolved.world_path,
        &resolved.resolved,
        &resolved.manifest_extras,
        resolved.suite.as_ref(),
    )?;
    if target.kind == WatchTargetKind::Driver {
        return Ok(WatchOutcome::MetadataOnly);
    }
    let mut specs = specs_for_target(&plan, target)?;
    let endpoint = crate::run::project_router_endpoint(project_root);
    crate::run::apply_session_connect(&mut plan, &mut specs, &endpoint);
    Ok(WatchOutcome::Revision { plan, specs })
}

fn specs_for_target(plan: &LaunchPlan, target: &WatchTarget) -> Result<Vec<ParticipantSpec>> {
    let wanted = target
        .participant_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    // `--watch` is an interactive dev-loop feature; there is no `AppContext`
    // this deep in the hot-reload swap path, so the terminal flag is
    // recomputed fresh rather than threaded through the watch loop.
    let ui = crate::Ui::from_env();
    let mut specs = Vec::new();
    for participant in plan
        .robots
        .iter()
        .flat_map(|robot| robot.participants.iter())
        .filter(|participant| {
            target.key == "plan-inputs"
                || wanted.contains(participant.launch.participant_id.as_str())
        })
    {
        if let Some(spec) = source_spec_from_launch_record(participant, &ui)? {
            specs.push(spec);
        }
    }
    Ok(specs)
}

pub(crate) fn watch_targets_from_sources(
    mode: WatchMode,
    _resolved: &ResolvedRobot,
    source_participants: &[SourceParticipant],
    live_ids: &BTreeSet<String>,
) -> Vec<WatchTarget> {
    let mut grouped = BTreeMap::<(WatchTargetKind, PathBuf, String), WatchTarget>::new();
    for participant in source_participants {
        let kind = participant.kind.shared_kind();
        if !watch_kind_in_mode(kind, mode, participant.name.as_str(), live_ids) {
            continue;
        }
        let artifact = participant.expected_artifact_id.clone();
        let key = (kind, participant.crate_dir.clone(), artifact.clone());
        let target = grouped.entry(key.clone()).or_insert_with(|| WatchTarget {
            key: format!("{}:{}:{}", kind.label(), key.2, key.1.display()),
            kind,
            label: target_label(kind, participant),
            crate_dir: participant.crate_dir.clone(),
            participant_ids: Vec::new(),
            board_ids: Vec::new(),
        });
        if matches!(kind, WatchTargetKind::Service | WatchTargetKind::Driver) {
            push_unique(&mut target.participant_ids, participant.name.clone());
        }
        push_unique(&mut target.board_ids, board_id_for(participant));
    }
    grouped.into_values().collect()
}

fn watch_kind_in_mode(
    kind: WatchTargetKind,
    mode: WatchMode,
    participant_id: &str,
    live_ids: &BTreeSet<String>,
) -> bool {
    match (mode, kind) {
        (_, WatchTargetKind::Service) => live_ids.contains(participant_id),
        (WatchMode::Run, WatchTargetKind::Driver) => live_ids.contains(participant_id),
        (WatchMode::Sim, WatchTargetKind::Driver) => true,
        (_, WatchTargetKind::Tool | WatchTargetKind::Simulator) => true,
    }
}

fn target_label(kind: WatchTargetKind, participant: &SourceParticipant) -> String {
    match kind {
        WatchTargetKind::Service => participant.name.clone(),
        WatchTargetKind::Driver => format!("driver-{}", participant.expected_artifact_id),
        WatchTargetKind::Tool => participant.name.clone(),
        WatchTargetKind::Simulator => format!("simulator-{}", participant.expected_artifact_id),
    }
}

fn board_id_for(participant: &SourceParticipant) -> String {
    match participant.kind {
        SourceParticipantKind::Simulator => {
            format!("simulator-{}", participant.expected_artifact_id)
        }
        _ => participant.name.clone(),
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::{ParticipantState, ParticipantStatus};

    #[test]
    fn plan_input_hash_detects_manifest_only_changes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let manifest = temp.path().join("robot.yaml");
        std::fs::write(&manifest, "schema: robot/v0\n")?;
        let target = plan_input_target(temp.path(), &[]);
        let before = hash_watch_target(&target)?;
        std::fs::write(&manifest, "schema: robot/v0\nrobot: {}\n")?;
        assert_ne!(before, hash_watch_target(&target)?);
        Ok(())
    }

    #[test]
    fn debounce_collapses_save_bursts() {
        let start = Instant::now();
        let mut debounce = DebounceQueue::new(Duration::from_millis(100));
        debounce.note_change("mission", start);
        debounce.note_change("mission", start + Duration::from_millis(50));
        assert!(debounce.due(start + Duration::from_millis(120)).is_empty());
        assert_eq!(
            debounce.due(start + Duration::from_millis(151)),
            vec!["mission".to_string()]
        );
    }

    #[test]
    fn run_driver_target_groups_live_instances_by_owner_crate() {
        let sources = vec![
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive",
                "ddsm115",
                PathBuf::from("/tmp/driver"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "right_drive",
                "ddsm115",
                PathBuf::from("/tmp/driver"),
            ),
        ];
        let live_ids = ["left_drive".to_string(), "right_drive".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>();
        let targets =
            watch_targets_from_sources(WatchMode::Run, &empty_resolved(), &sources, &live_ids);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, WatchTargetKind::Driver);
        assert_eq!(
            targets[0].participant_ids,
            vec!["left_drive".to_string(), "right_drive".to_string()]
        );
    }

    #[test]
    fn sim_driver_target_is_metadata_only_even_without_live_instance() {
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive",
            "ddsm115",
            PathBuf::from("/tmp/driver"),
        )];
        let targets = watch_targets_from_sources(
            WatchMode::Sim,
            &empty_resolved(),
            &sources,
            &BTreeSet::new(),
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, WatchTargetKind::Driver);
        assert_eq!(targets[0].participant_ids, vec!["left_drive".to_string()]);
    }

    #[tokio::test]
    async fn failed_watch_check_does_not_send_swap_action() {
        let board = BoardBackend::new();
        board.upsert(ParticipantStatus::new(
            "mission",
            ParticipantKind::Service,
            ParticipantState::Ready,
        ));
        let (tx, mut rx) = mpsc::channel(1);
        let target = WatchTarget {
            key: "service:mission:/tmp/mission".to_string(),
            kind: WatchTargetKind::Service,
            label: "mission".to_string(),
            crate_dir: PathBuf::from("/tmp/mission"),
            participant_ids: vec!["mission".to_string()],
            board_ids: vec!["mission".to_string()],
        };

        apply_watch_result(
            Path::new("/tmp"),
            &board,
            &tx,
            &target,
            Instant::now(),
            Err(anyhow!("graph check failed")),
        )
        .await;

        assert!(rx.try_recv().is_err());
        let snapshot = board.snapshot();
        let status = snapshot.participants.get("mission").expect("mission note");
        assert!(
            status
                .note
                .as_deref()
                .is_some_and(|note| note.contains("failed: graph check failed")),
            "{status:?}"
        );
    }

    #[tokio::test]
    async fn successful_watch_swap_sends_supervisor_action() {
        let board = BoardBackend::new();
        let (tx, mut rx) = mpsc::channel(1);
        let target = WatchTarget {
            key: "service:mission:/tmp/mission".to_string(),
            kind: WatchTargetKind::Service,
            label: "mission".to_string(),
            crate_dir: PathBuf::from("/tmp/mission"),
            participant_ids: vec!["mission".to_string()],
            board_ids: vec!["mission".to_string()],
        };
        let spec = ParticipantSpec {
            key: phoxal_cli_core::session::ProcessKey::project("mission"),
            id: "mission".to_string(),
            kind: phoxal_cli_core::session::ParticipantKind::Service,
            executable: PathBuf::from("/bin/echo"),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            process_group: true,
            note: None,
            bus_participant: true,
            readiness: ParticipantSpec::exact_liveliness_template(
                phoxal_cli_core::session::RobotKey::new("test", "robot"),
                "mission",
            ),
            startup_requirement: phoxal_cli_core::session::StartupRequirement::Required,
            runtime_failure: phoxal_cli_core::session::RuntimeFailurePolicy::StopProject,
            restart_policy: Default::default(),
        };

        apply_watch_result(
            Path::new("/tmp"),
            &board,
            &tx,
            &target,
            Instant::now(),
            Ok(WatchOutcome::Revision {
                plan: LaunchPlan {
                    mode: LaunchMode::Run,
                    site: Vec::new(),
                    robots: Vec::new(),
                },
                specs: vec![spec],
            }),
        )
        .await;

        let action = rx.try_recv().expect("swap action");
        let SupervisorAction::ReconcilePlan {
            revision,
            specs,
            note,
            ..
        } = action
        else {
            panic!("expected swap action");
        };
        assert_eq!(revision.number, 1);
        assert_eq!(
            specs[0].key,
            phoxal_cli_core::session::ProcessKey::project("mission")
        );
        assert!(note.contains("restarted"), "{note}");
    }

    fn empty_resolved() -> ResolvedRobot {
        let robot = phoxal::model::robot::v0::Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: robot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: []
    right_actuators: []
    left_encoders: []
    right_encoders: []
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components: {}
"#,
        )
        .unwrap();
        ResolvedRobot {
            robot,
            train: "0.36.0".to_string(),
            target: "host".to_string(),
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        }
    }
}
