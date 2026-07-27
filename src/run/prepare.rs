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
use phoxal_cli_core::project::launch_plan::PlanContext;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::layout::RuntimeLayout;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::discover_robot_yaml;
use phoxal_cli_core::project::resolver::load_robot;
use phoxal_cli_core::session::{ProcessKey, StartupRequirement};
use std::collections::BTreeMap;
use std::path::Path;

/// The output of one staging refresh over a buildable source project: the
/// resolved graph and its source-participant records the staging step consumed,
/// the resolved driver policy, and the staged runtime layout root every
/// executable and the launch plan are then read from. Everything after staging -
/// plan construction, spec building, `phoxal build`'s archive - reads only the
/// staged layout; these fields are the staging-side inputs the source path still
/// needs (crate directories for cwd/rebuild, the resolved graph for router
/// config).
pub(crate) struct StagedProject {
    pub(crate) robot_path: std::path::PathBuf,
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) resolved: ResolvedRobot,
    pub(crate) source_participants: Vec<phoxal_cli_core::check::source::SourceParticipant>,
    pub(crate) driver_policy: DriverPolicy,
    /// The staged runtime layout root - `.phoxal/bundle/`.
    pub(crate) staged_root: std::path::PathBuf,
}

impl StagedProject {
    /// The plan-construction options this refresh resolved, so `run`, `start`,
    /// and `build` all validate/construct the plan against the identical
    /// driver policy the staging step honored.
    pub(crate) fn plan_options(&self) -> phoxal_cli_core::project::layout::PlanOptions {
        phoxal_cli_core::project::layout::PlanOptions {
            drivers: self.driver_policy.selection(),
        }
    }
}

/// Refresh the host-triple staging for a buildable source project and return the
/// staged layout plus the staging-side inputs (#936). This is the one staging
/// entry `run`, `start`, and `phoxal build` all share, so they build and
/// stage identically before diverging on what they do with the staged layout:
/// resolve the locked graph, prepare native artifacts, resolve the driver policy,
/// stage the runtime layout under `.phoxal/bundle/`, run the
/// source-time check, and complete the flat `bin/` store. It never constructs the
/// launch plan or touches the supervisor - the caller runs
/// `loader::validate_layout_plan` over `staged_root` next.
///
/// `build` reuses this per target triple: `build` is a
/// native-bundle [`StagingBuild`](crate::run::StagingBuild) carrying the
/// requested `--target`,
/// which threads through the resolve/stage/`bin/`-completion steps so the same
/// code cross-compiles (or reuses container-built) workspace crates and links
/// the suite's per-target official blobs into `.phoxal/bundle/`. `run`,
/// and `start` pass `StagingBuild::host_runtime()`.
pub(crate) fn refresh_staging(
    project_start: &Path,
    options: &RunOptions,
    build: &crate::run::StagingBuild,
    check_source: bool,
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
        },
    )?;

    let staged_root = crate::stager::stage_runtime_layout(project_root, &resolved)
        .context("failed to stage the runtime layout")?;

    // Materialize every official service, tool, and the infrastructure
    // router into the staged `bin/` up front, via `cargo install`
    // (organization#951 WS4). This is what makes the source check below able
    // to read every official's embedded metadata straight off disk, and
    // completes the flat `bin/` store the loader requires.
    crate::stager::materialize_official_store(
        &staged_root,
        &resolved,
        options.offline,
        |crate_dir, name| build.build_user_binary(crate_dir, name, ui),
    )
    .context("failed to materialize official runtimes")?;

    let source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;

    // Source/staging-time validation: build every source participant (for its
    // embedded metadata) and check the source graph before we stage and run.
    // Execution-time validation is the loader's, over the staged layout below.
    //
    // `phoxal build` skips this host-native pass (`check_source == false`): a
    // cross or container target's Linux-only crates need not compile on the
    // build host, and the loader's target-aware validation over the staged
    // (cross-built) binaries is the authoritative check for a bundle (#936).
    if check_source {
        run_source_check(&staged_root, &robot, &resolved, &source_participants)?;
    }

    // Complete the staged `bin/` store so the loader can inspect every required
    // runtime off-disk. This is the last step that consumes the resolved graph;
    // everything after it reads only the staged layout.
    crate::run::stage_complete_bin_store(
        &staged_root,
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

    Ok(StagedProject {
        // `project_root` borrows `robot_path`, so clone rather than move it.
        robot_path: robot_path.clone(),
        project_root: project_root.to_path_buf(),
        resolved,
        source_participants,
        driver_policy,
        staged_root,
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
        ui,
    )?;
    let plan_options = staged.plan_options();

    // The one execution path: construct and validate the launch plan from the
    // staged layout alone. Byte-identical, for the same robot, to a plan built
    // from an extracted bundle of this layout.
    crate::runtime_header::RuntimeHeader::read_and_validate(&staged.staged_root)?;
    let plan = crate::loader::validate_layout_plan(
        &staged.staged_root,
        &plan_options,
        phoxal_cli_core::project::layout::LayoutInspection::Host,
        run,
    )
    .context("failed to construct the launch plan from the staged runtime layout")?;

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
/// nothing to build, resolve, or fetch: the launch plan and every executable
/// come from the layout's flat `bin/` store, so this needs no Cargo, suite,
/// toolchain, or network, and never touches `.phoxal/artifacts`. An arbitrary
/// layout keeps runtime state under `<layout_root>/.phoxal`; the installed
/// `/var/phoxal` identity maps persistent state to `/var/lib/phoxal/state` and
/// sockets to `/run/phoxal`.
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

/// The source-time graph check: build every source participant's binary for its
/// embedded metadata and validate the source graph, failing the run if the
/// train's check gate rejects it. This is a staging-side gate; the loader
/// re-validates config over the staged layout.
fn run_source_check(
    staged_root: &Path,
    robot: &phoxal::model::robot::v0::Robot,
    resolved: &ResolvedRobot,
    source_participants: &[phoxal_cli_core::check::source::SourceParticipant],
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
        build_participant_report_from_source,
    )?;
    if !outcome.is_ok() {
        crate::check::ensure_check_outcome_ok(&outcome)?;
    }
    Ok(())
}
