use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::run::{RunOptions, spec_from_launch_record};
use crate::simulation::{
    SimulateMode, SimulateOptions, build_checked_sim_launch_plan, resolve_project,
};
use crate::supervisor::{BoardBackend, ParticipantSpec, SupervisorAction};
use phoxal_cli_core::check::source::{SourceParticipant, SourceParticipantKind};
use phoxal_cli_core::project::launch_plan::{LaunchPlan, PlanContext};
use phoxal_cli_core::project::resolver::ResolvedRobot;
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
    // Watch is spawned only for source runs (`--watch` is rejected on layout
    // roots before this point), so the source graph is always present.
    let source = config
        .ctx
        .source
        .as_ref()
        .expect("--watch runs only on source projects");
    let mut targets = watch_targets_from_sources(
        WatchMode::Run,
        &source.resolved,
        &source.source_participants,
        &config.live_ids,
    );
    targets.push(plan_input_target(
        &config.ctx.project_root,
        &config.live_ids,
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
    // Simulation always prepares from a source project.
    let source = config
        .ctx
        .source
        .as_ref()
        .expect("simulation always prepares from a source project");
    let mut targets = watch_targets_from_sources(
        WatchMode::Sim,
        &source.resolved,
        &source.source_participants,
        &config.live_ids,
    );
    targets.push(plan_input_target(
        &config.ctx.project_root,
        &config.live_ids,
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

fn plan_input_target(project_root: &Path, live_ids: &BTreeSet<String>) -> WatchTarget {
    let ids = live_ids.iter().cloned().collect::<Vec<_>>();
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
    let mut files = Vec::new();
    collect_plan_input_files(&target.crate_dir, &target.crate_dir, &mut files)?;
    files.sort();
    let mut digest = Sha256::new();
    for path in files {
        let relative = path
            .strip_prefix(&target.crate_dir)
            .expect("collected plan input is below project root");
        digest.update(relative.as_os_str().as_encoded_bytes());
        digest.update(std::fs::read(&path)?);
    }
    Ok(hex::encode(digest.finalize()))
}

fn collect_plan_input_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if matches!(
                entry.file_name().to_str(),
                Some(".git" | ".phoxal" | ".claude" | "target")
            ) {
                continue;
            }
            collect_plan_input_files(root, &path, files)?;
        } else if file_type.is_file()
            && matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("yaml" | "yml" | "toml" | "lock" | "json" | "urdf" | "wbt")
            )
        {
            debug_assert!(path.starts_with(root));
            files.push(path);
        }
    }
    Ok(())
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
    // A hot-reload rebuilds and re-stages through the exact staging entry the
    // live run used, then derives the plan from that layout alone - the one
    // execution path (#936). The changed crate is rebuilt and re-staged into
    // `bin/` (source-time check included) before the loader inspects it, so the
    // layout-derived plan and its coherence check see the fresh metadata, and
    // the identical driver policy is honored across restages.
    let ui = crate::Ui::from_env();
    let staged = crate::run::refresh_staging(
        project_root,
        options,
        &crate::run::StagingBuild::local(None),
        true,
        &ui,
    )?;
    let mut plan = crate::loader::validate_layout_plan(
        &staged.staged_root,
        &staged.plan_options(),
        phoxal_cli_core::project::layout::LayoutInspection::Host,
    )
    .context("failed to construct the launch plan from the staged runtime layout")?;
    let mut specs = specs_for_target(&plan, &staged.resolved, &staged.project_root, target)?;
    let endpoint = crate::run::project_router_endpoint(&staged.project_root);
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
        resolved.suite.as_ref(),
    )?;
    if target.kind == WatchTargetKind::Driver {
        return Ok(WatchOutcome::MetadataOnly);
    }
    let mut specs = specs_for_target(&plan, &resolved.resolved, &resolved.project_root, target)?;
    let endpoint = crate::run::project_router_endpoint(project_root);
    crate::run::apply_session_connect(&mut plan, &mut specs, &endpoint);
    Ok(WatchOutcome::Revision { plan, specs })
}

fn specs_for_target(
    plan: &LaunchPlan,
    resolved: &ResolvedRobot,
    project_root: &Path,
    target: &WatchTarget,
) -> Result<Vec<ParticipantSpec>> {
    let wanted = target
        .participant_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    // A watch rebuild re-stages every swapped binary into the same staged
    // runtime layout the live run executes from, so a hot-reloaded participant
    // is resolved from `bin/` exactly like the original launch (#936).
    let staged_root = crate::stager::layout_path(project_root, resolved);
    // The staging-side record of source crate directories the source-free plan
    // no longer carries (#936): a hot-reloaded user service or workspace driver
    // is rebuilt from here, exactly like the original staging pass.
    let source_participants = crate::check::source_participants_from_resolved(
        project_root,
        resolved,
        crate::component_driver::component_driver_crate_dir,
    )?;
    let source_dirs = crate::run::source_dirs_by_participant(&source_participants);
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
        if let Some(spec) =
            spec_from_launch_record(participant, resolved, &source_dirs, &staged_root)?
        {
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
        let target = plan_input_target(temp.path(), &BTreeSet::new());
        let before = hash_watch_target(&target)?;
        std::fs::write(&manifest, "schema: robot/v0\nrobot: {}\n")?;
        assert_ne!(before, hash_watch_target(&target)?);
        Ok(())
    }

    #[test]
    fn plan_input_hash_detects_nested_manifests_and_urdf() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let component = temp.path().join("components/drive");
        std::fs::create_dir_all(&component)?;
        let manifest = component.join("component.yaml");
        let urdf = component.join("structure.urdf");
        std::fs::write(&manifest, "schema: component/v0\n")?;
        std::fs::write(&urdf, "<robot/>")?;
        let target = plan_input_target(temp.path(), &BTreeSet::new());
        let before = hash_watch_target(&target)?;
        std::fs::write(&urdf, "<robot name=\"changed\"/>")?;
        assert_ne!(before, hash_watch_target(&target)?);
        let after_urdf = hash_watch_target(&target)?;
        std::fs::write(&manifest, "schema: component/v0\ncomponent: {}\n")?;
        assert_ne!(after_urdf, hash_watch_target(&target)?);
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
                    mode: phoxal_cli_core::project::launch_plan::LaunchMode::Run,
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
