//! Prepare responsibilities for run.
//!
//! `run` is universal (#936): it prepares the same [`PreparedRun`] whether the
//! root is a buildable source project or an already-staged runtime layout (an
//! extracted `build.phoxal` or a `.phoxal/bundle/` directory). Both end
//! at the one execution path - [`RuntimeLayout::construct_plan`], surfaced here
//! through `loader::validate_layout_plan` - which derives the launch graph from
//! the staged layout alone. The two entry points differ only in the staging
//! step before it: a source root resolves, checks, and stages; a layout root
//! has nothing to build and runs in place.

use super::{DriverPolicy, PreparedRun, RunOptions};
use crate::check::CheckGraphContext;
use crate::check::build_participant_report_from_source;
use crate::check::check_artifact_refs_from_resolved;
use crate::check::extract_participant_report_from_staged_runtime;
use crate::check::extract_participant_report_from_staged_tool;
use crate::check::fetch_participant_report_from_tool;
use crate::check::run_check_with_context;
use crate::check::source_participants_from_resolved;
use crate::check::tool_participants_from_resolved;
use crate::component_driver::component_driver_crate_dir;
use crate::resolver::resolve;
use crate::supervisor::BoardBackend;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::check::participant_metadata::expected_target_for_triple;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::PlanContext;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::layout::{LayoutInspection, PlanOptions, RuntimeLayout};
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::discover_robot_yaml;
use phoxal_cli_core::project::resolver::load_robot;
use phoxal_cli_core::session::{ProcessKey, StartupRequirement};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

/// The output of one staging refresh over a buildable source project: the
/// resolved graph and its source-participant records the staging step consumed,
/// the resolved driver policy, and the staged runtime layout root every
/// executable and the launch plan are then read from. Everything after staging -
/// spec building, `phoxal build`'s archive - reads only the
/// staged layout; these fields are the staging-side inputs the source path still
/// needs (crate directories for cwd/rebuild, the resolved graph for router
/// config). `plan` is the loader's own validated launch plan, already
/// constructed against `staged_root` BEFORE it was published (#951 WS4
/// review) - callers must reuse it rather than re-running
/// `loader::validate_layout_plan`, which would be a second, redundant
/// validation pass over already-validated bytes.
pub(crate) struct StagedProject {
    pub(crate) robot_path: std::path::PathBuf,
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) resolved: ResolvedRobot,
    pub(crate) source_participants: Vec<phoxal_cli_core::check::source::SourceParticipant>,
    pub(crate) driver_policy: DriverPolicy,
    /// The staged runtime layout root - `.phoxal/bundle/`.
    pub(crate) staged_root: std::path::PathBuf,
    pub(crate) plan: LaunchPlan,
}

/// Refresh the host-triple staging for a buildable source project and return the
/// staged layout plus the staging-side inputs (#936). This is the one staging
/// entry `run`, `start`, and `phoxal build` all share, so they build and
/// stage identically before diverging on what they do with the staged layout:
/// resolve the locked graph, materialize officials, resolve the driver policy,
/// stage the runtime layout, run the source-time check, complete the flat
/// `bin/` store, and validate the whole compiled layout through the loader -
/// ALL of it against an unpublished candidate directory, exactly once, and
/// ONLY THEN publish it as the live `.phoxal/bundle/` (organization#951 WS4
/// review: the previous ordering published first and materialized/validated
/// after, so any failure left the robot with its previous working bundle
/// deleted and the live one empty or half-populated). A failure anywhere in
/// this function therefore never touches the previous live bundle at all.
///
/// `build` reuses this per target triple: `build` is a
/// native-bundle [`StagingBuild`](crate::run::StagingBuild) carrying the
/// requested `--target`,
/// which threads through the resolve/stage/`bin/`-completion steps so the same
/// code cross-compiles (or reuses container-built) workspace crates and links
/// the official set for that target. `run`,
/// and `start` pass `StagingBuild::host_runtime()`.
pub(crate) fn refresh_staging(
    project_start: &Path,
    options: &RunOptions,
    build: &crate::run::StagingBuild,
    check_source: bool,
    run: RunIdentity,
    ui: &crate::Ui,
) -> Result<StagedProject> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let robot = load_robot(&robot_path)?;

    // The driver policy is resolved from the parsed robot BEFORE resolution
    // (#936, finding 2): it must gate resolution itself, so an excluded
    // driver is never resolved, materialized, staged, required, inspected,
    // or planned. It threads through both staging and plan construction from
    // here.
    let driver_policy = DriverPolicy::from_options(options, &crate::run::driven_instances(&robot))?;

    // A cross `--target` resolves official packages for that target (the
    // same per-target resolution `phoxal build --target` performs); a host
    // pass leaves both `None` so resolution targets the host triple.
    let official_target = build.target().map(str::to_string);
    let resolved = resolve(
        &robot,
        project_root,
        ResolveOptions {
            official_target_triple: official_target.clone(),
            tool_target_triple: official_target,
            // Finding 1 (#936): the driver policy gates resolution itself - an
            // excluded driver is not resolved, so it cannot enter the source
            // check or be built/materialized.
            drivers: driver_policy.selection(),
            // Native runtime bundles deliberately exclude operator-host Webots
            // simulators. Host run/start staging keeps them.
            include_simulators: build.include_simulators(),
            offline: options.offline,
        },
    )?;

    // Stage into an UNPUBLISHED candidate. Every install, source build,
    // metadata read, and loader validation below runs against
    // `candidate.path()`; only the final `publish_runtime_layout` call at the
    // bottom of this function ever touches the live `.phoxal/bundle/`.
    let candidate = crate::stager::begin_runtime_layout(project_root, &resolved)
        .context("failed to stage the runtime layout")?;

    // Materialize every official service, tool, and the infrastructure
    // router into the candidate `bin/` up front, via `cargo install`
    // (organization#951 WS4). This is what makes the source check below able
    // to read every official's embedded metadata straight off disk, and
    // completes the flat `bin/` store the loader requires.
    crate::stager::materialize_official_store(
        candidate.path(),
        &resolved,
        options.offline,
        build.officials_source(),
        |crate_dir, name| build.build_user_binary(crate_dir, name, ui, options.offline),
    )
    .context("failed to materialize official runtimes")?;

    let source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;

    // Source/staging-time validation: build every source participant (for its
    // embedded metadata) and check the source graph before we stage and run.
    // Execution-time validation is the loader's, over the candidate layout
    // below.
    //
    // `phoxal build` skips this host-native pass (`check_source == false`): a
    // cross or container target's Linux-only crates need not compile on the
    // build host, and the loader's target-aware validation over the staged
    // (cross-built) binaries is the authoritative check for a bundle (#936).
    if check_source {
        run_source_check(
            candidate.path(),
            &robot,
            &resolved,
            &source_participants,
            options.offline,
        )?;
    }

    // Complete the candidate `bin/` store so the loader can inspect every
    // required runtime off-disk. This is the last step that consumes the
    // resolved graph; everything after it reads only the candidate layout.
    crate::run::stage_complete_bin_store(
        candidate.path(),
        &resolved,
        &source_participants,
        &driver_policy.selection(),
        options.offline,
        build,
        ui,
    )?;

    // Declaration drift (#950) is warned from THIS shared path, so run,
    // start and build all surface it exactly once.
    crate::run::report_undeclared_runtimes(&resolved.undeclared_runtimes, ui);

    // The loader's own execution-time validation - config-schema pairing and
    // architecture inspection - runs against the candidate too, still before
    // publish. `run`/`start` inspect against the host; a `--target` build
    // inspects against the declared target signature instead, since the
    // staged binaries were cross-compiled (or container-built) for it, not
    // for this host.
    crate::runtime_header::RuntimeHeader::read_and_validate(candidate.path())?;
    let plan_options = PlanOptions {
        drivers: driver_policy.selection(),
    };
    let inspection = match build.target() {
        Some(target) => {
            LayoutInspection::Target(expected_target_for_triple(target).with_context(|| {
                format!("cannot validate the staged runtime layout for target {target}")
            })?)
        }
        None => LayoutInspection::Host,
    };
    let mut plan =
        crate::loader::validate_layout_plan(candidate.path(), &plan_options, inspection, run)
            .context("failed to validate the staged runtime layout")?;

    // Every install, build, check, and validation above succeeded against the
    // candidate alone - publish it as the live layout now, and only now.
    let candidate_path = candidate.path().to_path_buf();
    let staged_root = crate::stager::publish_runtime_layout(candidate, &resolved)
        .context("failed to publish the staged runtime layout")?;

    // The plan above was constructed and validated against the unpublished
    // candidate - `validate_layout_plan` opened the layout at
    // `candidate.path()`, so every participant's `ParticipantLaunch::robot_root`
    // (the `--robot-root` every launched process receives) was baked in as the
    // candidate path. The rename just above made `staged_root` the live layout
    // and the candidate path no longer exists, so every one of those recorded
    // roots is now dangling. Repoint them to the published root - the same fix
    // `simulate` already applies to its specs' `executable`/`cwd`
    // (`repoint_after_publish`), for the identical reason: both stage against a
    // path that publish then renames away.
    repoint_plan_robot_roots(&mut plan, &candidate_path, &staged_root);

    Ok(StagedProject {
        // `project_root` borrows `robot_path`, so clone rather than move it.
        robot_path: robot_path.clone(),
        project_root: project_root.to_path_buf(),
        resolved,
        source_participants,
        driver_policy,
        staged_root,
        plan,
    })
}

/// Prepare a run from a buildable source project: refresh the staged runtime
/// layout through the shared [`refresh_staging`] entry, then construct and
/// validate the launch plan from that staged layout. The plan and every
/// executable come from the staged layout, never the resolved graph directly
/// (#936) - the resolved graph is a staging-side input only.
pub(crate) fn prepare_run_on_board(
    project_start: &Path,
    options: RunOptions,
    ui: &crate::Ui,
    board: BoardBackend,
    run: RunIdentity,
) -> Result<PreparedRun> {
    let staged = refresh_staging(
        project_start,
        &options,
        &crate::run::StagingBuild::host_runtime(),
        true,
        run,
        ui,
    )?;

    // The one execution path: `refresh_staging` already constructed and
    // validated the launch plan against the candidate BEFORE publishing it
    // (organization#951 WS4 review) - reuse it rather than re-validating the
    // now-published bytes a second time. Byte-identical, for the same robot,
    // to a plan built from an extracted bundle of this layout.
    let plan = staged.plan.clone();

    board.configure(
        staged.project_root.display().to_string(),
        staged.resolved.train.clone(),
        "run",
    );
    register_router_process(&board);
    // Explain any policy-excluded drivers as a session-level advisory: they are
    // never plan participants, so this summary is the only signal an operator
    // gets for why hardware rows are absent (#936, finding 8).
    crate::run::report_excluded_drivers(
        &staged.driver_policy,
        &crate::run::driven_instances(&staged.resolved.robot),
        ui,
    );

    let mut specs = Vec::new();
    // The staging-side record of source crate directories the source-free plan
    // no longer carries: a participant built from local source runs from its
    // crate directory (relative asset resolution) and is rebuilt there under
    // Execution identity always comes from the plan's `bin/` name.
    let source_dirs = crate::run::source_dirs_by_participant(&staged.source_participants);
    // Single-pass execution (#936, finding 3): `refresh_staging` already built
    // and staged every participant binary into `bin/`, and `validate_layout_plan`
    // just validated that exact `bin/`. Build the specs by reading those
    // already-validated bytes - the SAME path an extracted bundle takes - instead
    // of re-resolving and rebuilding each participant. That second pass could
    // rebuild between validation and launch and execute bytes that were never
    // validated; there is now exactly one resolution+staging pass. Source-only
    // metadata (the crate cwd) is carried through `source_cwd` without touching
    // binary resolution.
    let layout = RuntimeLayout::open(&staged.staged_root).with_context(|| {
        format!(
            "failed to open staged runtime layout {}",
            staged.staged_root.display()
        )
    })?;
    let cwd_for = |participant: &phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord| {
        crate::run::source_cwd(participant, &staged.resolved, &source_dirs)
    };
    crate::run::build_layout_specs(&plan, &layout, &board, &mut specs, &cwd_for)?;

    // Resolve the router config from the STAGED layout, not the source tree:
    // staging copies `router.config` into the layout under its relative path, so
    // a source run and an extracted `build.phoxal` resolve the identical staged
    // asset (#936, finding 4).
    let router_config =
        crate::run::resolve_router_config(&staged.resolved.robot, &staged.staged_root)?;
    let robot_targets = super::RobotFeedTarget::from_plan(&plan);
    let StagedProject {
        robot_path,
        project_root,
        resolved,
        source_participants,
        staged_root,
        ..
    } = staged;
    let ctx = PlanContext {
        robot_path,
        project_root,
        source: Some(phoxal_cli_core::project::launch_plan::PlanSource {
            resolved,
            source_participants,
        }),
    };

    Ok(PreparedRun {
        ctx,
        robot_targets,
        plan,
        board,
        specs,
        staged_root,
        router_config,
    })
}

/// Prepare a run from an already-staged runtime layout at `layout_root` - an
/// extracted `build.phoxal` or a `.phoxal/bundle/` directory. There is
/// nothing to build, resolve, or materialize: the launch plan and every
/// executable come from the layout's flat `bin/` store, so this needs no
/// Cargo, toolchain, or network. An arbitrary layout keeps runtime state
/// under `<layout_root>/.phoxal`; the installed `/var/phoxal` identity maps
/// persistent state to `/var/lib/phoxal/state` and sockets to `/run/phoxal`.
pub(crate) fn prepare_layout_run_on_board(
    layout_root: &Path,
    options: RunOptions,
    board: BoardBackend,
    run: RunIdentity,
) -> Result<PreparedRun> {
    // A compiled root declares its whole typed-document contract before any
    // robot or participant metadata is interpreted.
    crate::runtime_header::RuntimeHeader::read_and_validate(layout_root)?;
    let layout = RuntimeLayout::open(layout_root).with_context(|| {
        format!(
            "failed to open staged runtime layout {}",
            layout_root.display()
        )
    })?;

    // The same driver policy the source path applies, so `--drivers off` runs an
    // extracted bundle on a host whose driver binaries it cannot inspect (#936):
    // excluded drivers are not required, resolved, inspected, or planned.
    let driver_policy =
        DriverPolicy::from_options(&options, &crate::run::driven_instances(layout.robot()))?;
    let plan_options = phoxal_cli_core::project::layout::PlanOptions {
        drivers: driver_policy.selection(),
    };
    let plan = crate::loader::validate_layout_plan(
        layout_root,
        &plan_options,
        phoxal_cli_core::project::layout::LayoutInspection::Host,
        run,
    )
    .context("failed to construct the launch plan from the staged runtime layout")?;

    board.configure(
        crate::runtime_paths::RuntimePaths::for_root(layout_root)
            .ownership_root
            .display()
            .to_string(),
        "staged".to_string(),
        "run",
    );
    register_router_process(&board);

    let mut specs = Vec::new();
    // An extracted bundle / staged layout has no source, so no participant has a
    // crate cwd - the closure always yields `None` (#936, finding 3).
    crate::run::build_layout_specs(&plan, &layout, &board, &mut specs, &|_| None)?;

    let router_config = crate::run::resolve_router_config(layout.robot(), layout_root)?;
    let robot_targets = super::RobotFeedTarget::from_plan(&plan);
    let ctx = PlanContext {
        robot_path: layout_root.join("robot.yaml"),
        project_root: layout_root.to_path_buf(),
        // A staged layout has no resolved source graph; source-needing
        // source consumers go through `PlanContext::source`,
        // which fails with an actionable error on this path.
        source: None,
    };

    Ok(PreparedRun {
        ctx,
        robot_targets,
        plan,
        board,
        specs,
        staged_root: layout_root.to_path_buf(),
        router_config,
    })
}

fn register_router_process(board: &BoardBackend) {
    board.upsert_process(
        ProcessKey::project("infrastructure-router"),
        crate::supervisor::ParticipantStatus::new(
            "infrastructure-router",
            phoxal_cli_core::session::ParticipantKind::Tool,
            crate::supervisor::ParticipantState::Starting,
        ),
        StartupRequirement::Required,
    );
}

/// Repoint every participant's `robot_root` from the unpublished candidate to
/// the published layout, in place. `construct_plan`/`construct_plan_from_selected`
/// (`crates/core/src/project/layout/plan.rs`) set `robot_root` to the exact
/// root the plan was constructed against - the candidate, here - so every
/// participant carries it, never only some.
pub(crate) fn repoint_plan_robot_roots(plan: &mut LaunchPlan, candidate: &Path, published: &Path) {
    for robot in &mut plan.robots {
        for participant in &mut robot.participants {
            if let Some(robot_root) = participant.launch.robot_root.as_mut() {
                repoint_after_publish(robot_root, candidate, published);
            }
        }
    }
}

/// Rewrite `path` from the candidate root to the published root when it falls
/// under the candidate at all - a source participant's `cwd` is its own crate
/// directory, never under either, and is correctly left untouched. `fs::rename`
/// (the publish step) preserves the relative structure exactly, so this prefix
/// swap is exact, never an approximation. Shared by `run`
/// ([`repoint_plan_robot_roots`], above) and `simulate`
/// (`simulation::setup::live_simulate_setup`, which repoints each spec's
/// `executable`/`cwd`) - both stage against an unpublished candidate and must
/// repoint every candidate-derived path once publish renames it away.
pub(crate) fn repoint_after_publish(path: &mut PathBuf, candidate: &Path, published: &Path) {
    if let Ok(relative) = path.strip_prefix(candidate) {
        *path = published.join(relative);
    }
}

/// The source-time graph check: build every source participant's binary for its
/// embedded metadata and validate the source graph, failing the run if the
/// train's check gate rejects it. This is a staging-side gate; the loader
/// re-validates config over the staged layout.
fn run_source_check(
    staged_root: &Path,
    robot: &phoxal::model::robot::v0::Robot,
    resolved: &ResolvedRobot,
    source_participants: &[phoxal_cli_core::check::source::SourceParticipant],
    offline: bool,
) -> Result<()> {
    let bin_dir = staged_root.join("bin");
    let platform_refs = check_artifact_refs_from_resolved(resolved);
    let tool_participants = tool_participants_from_resolved(resolved)?;
    let mut official_by_name = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| {
            (
                phoxal_cli_core::project::resolver::official_binary_name(
                    runtime.kind,
                    &runtime.name,
                ),
                runtime,
            )
        })
        .collect::<BTreeMap<_, _>>();
    official_by_name.extend(crate::check::component_driver_runtimes_by_ref(resolved));
    let tools_by_name = resolved
        .tools
        .iter()
        .map(|tool| (tool.binary_name.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        source_participants,
        CheckGraphContext { robot: Some(robot) },
        |binary_name| {
            if let Some(runtime) = official_by_name.get(binary_name) {
                return extract_participant_report_from_staged_runtime(&bin_dir, runtime);
            }
            if let Some(tool) = tools_by_name.get(binary_name) {
                return extract_participant_report_from_staged_tool(&bin_dir, tool);
            }
            Err(anyhow!(
                "resolved official artifact {binary_name} was not materialized into bin/"
            ))
        },
        fetch_participant_report_from_tool,
        |participant| build_participant_report_from_source(participant, offline),
    )?;
    if !outcome.is_ok() {
        crate::check::ensure_check_outcome_ok(&outcome)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::check::participant_metadata::{host_architecture, host_binary_format};
    use phoxal_cli_core::project::layout::{DriverSelection, RequiredRuntimeKind};

    const ROBOT_YAML: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;

    /// Synthesize a host-format object carrying the phoxal metadata section a
    /// required runtime's own identity must match (organization#957), so
    /// `RuntimeLayout::construct_plan` can inspect a real object shape off-disk
    /// with no actual binary built (mirrors `run::participants`' own fixture).
    fn synthesize_binary_with_id(id: &str) -> Vec<u8> {
        use object::write::Object;
        let format = host_binary_format();
        let (segment, name): (&[u8], &[u8]) = match format {
            object::BinaryFormat::MachO => (b"__DATA", b"__phoxal_meta"),
            _ => (b"", b".phoxal_meta"),
        };
        let mut obj = Object::new(format, host_architecture(), object::Endianness::Little);
        let section = obj.add_section(
            segment.to_vec(),
            name.to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        let payload = format!(r#"{{"id":"{id}","config_schema":{{"type":"null"}}}}"#);
        obj.append_section_data(section, payload.as_bytes(), 1);
        obj.write().expect("synthesize object file")
    }

    /// Stage a minimal but complete layout - compiled `robot.yaml` plus a
    /// synthesized binary for every runtime the loader requires - directly at
    /// `root`, mirroring what `stager::begin_runtime_layout` +
    /// `stage_complete_bin_store` leave behind in a real candidate directory,
    /// with no Cargo or network involved.
    fn stage_layout(root: &Path) -> Result<()> {
        std::fs::create_dir_all(root)?;
        std::fs::write(root.join("robot.yaml"), ROBOT_YAML)?;
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin)?;
        let layout = RuntimeLayout::open(root)?;
        for required in layout.required_runtimes(&DriverSelection::All) {
            if required.kind == RequiredRuntimeKind::Infrastructure {
                continue;
            }
            std::fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_with_id(&required.identity),
            )?;
        }
        Ok(())
    }

    /// The bug this module fixes: `refresh_staging` constructs and validates
    /// the launch plan against the unpublished candidate
    /// (`crate::loader::validate_layout_plan(candidate.path(), ...)`), which
    /// bakes the candidate root into every participant's
    /// `ParticipantLaunch::robot_root` (`crates/core/src/project/layout/plan.rs`,
    /// `construct_plan_from_selected`: `robot_root = self.root().to_path_buf()`).
    /// `publish_runtime_layout` then renames that candidate away, so every one
    /// of those recorded roots goes stale - the real symptom was every launched
    /// participant's `--robot-root` naming a `.bundle-candidate-*` directory
    /// that no longer existed. This fails if `repoint_plan_robot_roots` is not
    /// called, or is called with the wrong `(candidate, published)` pair.
    #[test]
    fn repoint_plan_robot_roots_leaves_no_participant_pointing_at_the_candidate() -> Result<()> {
        let project = tempfile::tempdir()?;
        // Named like a real staging candidate (`stager::begin_runtime_layout`
        // prefixes with `.bundle-candidate-`); the repoint itself only cares
        // about the prefix match, not the name.
        let candidate = project.path().join(".phoxal/.bundle-candidate-test0000");
        stage_layout(&candidate)?;

        let mut plan = RuntimeLayout::construct_plan(
            &candidate,
            &PlanOptions::default(),
            RunIdentity::default(),
        )?
        .plan;
        assert!(
            !plan.robots.is_empty()
                && plan
                    .robots
                    .iter()
                    .any(|robot| !robot.participants.is_empty()),
            "the fixture must produce a non-empty plan or this test proves nothing"
        );
        // Sanity: before repointing, construction really did bake in the
        // candidate root - otherwise this test would pass trivially.
        for participant in plan.robots.iter().flat_map(|robot| &robot.participants) {
            assert_eq!(
                participant.launch.robot_root.as_deref(),
                Some(candidate.as_path()),
                "{} must start out pointing at the candidate",
                participant.launch.participant_id
            );
        }

        let published = project.path().join(".phoxal/bundle");
        repoint_plan_robot_roots(&mut plan, &candidate, &published);

        for participant in plan.robots.iter().flat_map(|robot| &robot.participants) {
            let robot_root = participant.launch.robot_root.as_ref().unwrap_or_else(|| {
                panic!("{} lost its robot_root", participant.launch.participant_id)
            });
            assert!(
                !robot_root.starts_with(&candidate),
                "{} still references the unpublished candidate: {}",
                participant.launch.participant_id,
                robot_root.display()
            );
            assert_eq!(
                robot_root, &published,
                "{} did not repoint to the published layout",
                participant.launch.participant_id
            );
        }
        Ok(())
    }
}
