use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::commands::check::{
    CheckGraphContext, SourceParticipant, SourceParticipantKind, build_emit_apis_from_source,
    fetch_emit_apis_from_native_artifact, platform_artifact_refs_from_resolved,
    run_check_with_context, source_participants_from_resolved,
};
use crate::commands::run::{RunOptions, source_spec_from_launch_record};
use crate::commands::simulate::{
    SimulateMode, SimulateOptions, build_checked_sim_launch_plan, resolve_project,
};
use crate::component_driver::component_driver_crate_dir;
use crate::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, LaunchPlan, PlanContext, build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedRobot, discover_robot_yaml, load_robot_with_extras,
    load_robot_with_extras_and_overlays, resolve,
};
use crate::supervisor::{BoardBackend, ParticipantSpec, SupervisorAction};
use crate::utils::hash_tree;

const WATCH_POLL: Duration = Duration::from_millis(500);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(650);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchMode {
    Run,
    Sim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum WatchTargetKind {
    Service,
    Driver,
    Tool,
    Simulator,
}

impl WatchTargetKind {
    fn label(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Driver => "driver",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
        }
    }
}

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
    let targets = watch_targets_from_sources(
        WatchMode::Run,
        &config.ctx.resolved,
        &config.ctx.source_participants,
        &config.live_ids,
    );
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
    let targets = watch_targets_from_sources(
        WatchMode::Sim,
        &config.ctx.resolved,
        &config.ctx.source_participants,
        &config.live_ids,
    );
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
                match hash_tree(&target.crate_dir) {
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
        match hash_tree(&target.crate_dir) {
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

async fn handle_target_change(
    mode: WatchMode,
    project_root: PathBuf,
    options: WatchOptions,
    board: BoardBackend,
    action_tx: mpsc::Sender<SupervisorAction>,
    target: WatchTarget,
) {
    let started = Instant::now();
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

    apply_watch_result(&board, &action_tx, &target, started, result).await;
}

async fn apply_watch_result(
    board: &BoardBackend,
    action_tx: &mpsc::Sender<SupervisorAction>,
    target: &WatchTarget,
    started: Instant,
    result: Result<WatchOutcome>,
) {
    match result {
        Ok(WatchOutcome::Swaps(specs)) => {
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
            for spec in specs {
                let id = spec.id.clone();
                if action_tx
                    .send(SupervisorAction::Swap {
                        id,
                        spec,
                        note: note.clone(),
                    })
                    .await
                    .is_err()
                {
                    set_note_all(board, target, "watch stopped: supervisor channel closed");
                    return;
                }
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
    format!("{:.1}s", started.elapsed().as_secs_f32())
}

fn set_note_all(board: &BoardBackend, target: &WatchTarget, note: impl AsRef<str>) {
    let note = note.as_ref();
    for id in &target.board_ids {
        board.set_note(id, note);
    }
}

enum WatchOutcome {
    Swaps(Vec<ParticipantSpec>),
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
    let catalog = crate::commands::load_catalog_for_robot_from_source(
        options.catalog_source.clone(),
        project_root,
        loaded.robot.artifacts.channel,
        &loaded.extras,
    )?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            emit_update_notice: false,
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
    let platform_refs = platform_artifact_refs_from_resolved(&resolved);
    let official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        &platform_refs,
        &[],
        &source_participants,
        CheckGraphContext {
            manifest_extras: &loaded.extras,
        },
        |artifact_ref| {
            let runtime = official_by_ref.get(artifact_ref).ok_or_else(|| {
                anyhow!("resolved official artifact {artifact_ref} is not in the catalog")
            })?;
            fetch_emit_apis_from_native_artifact(runtime)
        },
        |_| unreachable!("run watch does not check site tools as graph participants"),
        build_emit_apis_from_source,
    )?;
    crate::commands::check::ensure_check_outcome_ok(&resolved.channel.to_string(), &outcome)?;
    let plan = build_launch_plan(
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
    Ok(WatchOutcome::Swaps(specs_for_target(&plan, target)?))
}

fn recheck_sim_target(
    project_root: &Path,
    options: &SimulateOptions,
    target: &WatchTarget,
) -> Result<WatchOutcome> {
    let resolved = resolve_project(project_root, options.clone(), SimulateMode::Live)?;
    let plan = build_checked_sim_launch_plan(
        &resolved.project_root,
        &resolved.world_path,
        &resolved.resolved,
        &resolved.manifest_extras,
        resolved.catalog.as_ref(),
    )?;
    if target.kind == WatchTargetKind::Driver {
        return Ok(WatchOutcome::MetadataOnly);
    }
    Ok(WatchOutcome::Swaps(specs_for_target(&plan, target)?))
}

fn specs_for_target(plan: &LaunchPlan, target: &WatchTarget) -> Result<Vec<ParticipantSpec>> {
    let wanted = target
        .participant_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let ui = crate::Ui;
    let mut specs = Vec::new();
    for participant in plan
        .robots
        .iter()
        .flat_map(|robot| robot.participants.iter())
        .filter(|participant| wanted.contains(participant.launch.participant_id.as_str()))
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
        let kind = match participant.kind {
            SourceParticipantKind::UserService | SourceParticipantKind::OfficialService => {
                WatchTargetKind::Service
            }
            SourceParticipantKind::ComponentDriver => WatchTargetKind::Driver,
            SourceParticipantKind::Tool => WatchTargetKind::Tool,
            SourceParticipantKind::Simulator => WatchTargetKind::Simulator,
        };
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
            id: "mission".to_string(),
            kind: crate::supervisor::ParticipantKind::UserService,
            executable: PathBuf::from("/bin/echo"),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(10),
            note: None,
        };

        apply_watch_result(
            &board,
            &tx,
            &target,
            Instant::now(),
            Ok(WatchOutcome::Swaps(vec![spec])),
        )
        .await;

        let action = rx.try_recv().expect("swap action");
        let SupervisorAction::Swap { id, note, .. } = action else {
            panic!("expected swap action");
        };
        assert_eq!(id, "mission");
        assert!(note.contains("restarted"), "{note}");
    }

    fn empty_resolved() -> ResolvedRobot {
        let robot = phoxal::model::robot::v0::Robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: robot
  namespace: test
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
            channel: crate::catalog::SelectionChannel::Stable,
            target: "host".to_string(),
            catalog_snapshot: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        }
    }
}
