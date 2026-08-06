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
use crate::build::cargo::{SourceArtifacts, build_selected_source_artifacts};
use crate::build::profile::StagingBuild;
use crate::resolve::project::resolve_with_train;
use crate::run::participants::{
    build_layout_specs, source_cwd, source_dirs_by_participant, stage_complete_bin_store,
};
use crate::run::report::{driven_instances, report_excluded_drivers, report_undeclared_runtimes};
use crate::validation::CheckGraphContext;
use crate::validation::build_participant_report_from_binary;
use crate::validation::check_artifact_refs_from_resolved;
use crate::validation::extract_participant_report_from_staged_runtime;
use crate::validation::run_check_with_context;
use crate::validation::source_participants_from_resolved;
use crate::{PrepareRunRequest, PreparedExecution, PreparedRouter};
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::check::participant_metadata::expected_target_for_triple;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::layout::{LayoutInspection, RuntimeLayout};
use phoxal_cli_core::project::resolver::BundlePlan;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::discover_robot_yaml;
use phoxal_cli_core::project::resolver::load_robot;
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
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) resolved: BundlePlan,
    pub(crate) source_participants: Vec<phoxal_cli_core::check::source::SourceParticipant>,
    pub(crate) driver_policy: DriverPolicy,
    /// The staged runtime layout root - `.phoxal/bundle/`.
    pub(crate) staged_root: std::path::PathBuf,
    pub(crate) plan: LaunchPlan,
}

/// One source/package resolution plus the exact options and staging profile
/// that produced it, ready to be materialized into a runtime layout.
///
/// Container builds resolve before entering the container so they can include
/// registry component drivers, then pass this exact value into staging instead
/// of compiling the authored project a second time. `project_root` and compiled
/// asset paths may point into a temporary source snapshot, so that snapshot must
/// outlive this value and the staging operation that consumes it. The resolved
/// driver policy remains attached so materialization and launch-plan
/// construction cannot select a different driver set.
pub(crate) struct ResolvedStagingInput {
    project_root: PathBuf,
    resolved: BundlePlan,
    driver_policy: DriverPolicy,
    options: RunOptions,
    build: StagingBuild,
}

impl ResolvedStagingInput {
    pub(crate) fn resolved(&self) -> &BundlePlan {
        &self.resolved
    }

    /// Replace only the materialization half of a staging profile after an
    /// external builder has produced its binaries. Resolution-visible target
    /// and simulator choices must remain identical.
    pub(crate) fn set_materialization_build(&mut self, build: StagingBuild) -> Result<()> {
        anyhow::ensure!(
            self.build.target() == build.target()
                && self.build.include_simulators() == build.include_simulators(),
            "prebuilt staging profile does not match the resolved target and simulator selection"
        );
        self.build = build;
        Ok(())
    }
}

/// Resolve and compile the source documents exactly once, preserving the
/// options and resolution-visible staging profile for later materialization.
pub(crate) fn resolve_staging(
    project_start: &Path,
    options: RunOptions,
    build: StagingBuild,
    ui: &dyn crate::Reporter,
) -> Result<ResolvedStagingInput> {
    resolve_staging_with_registry_cache(project_start, None, options, build, ui)
}

/// Resolve a frozen source tree while directing immutable registry archives to
/// an explicitly owned live cache (the container builder's project root).
pub(crate) fn resolve_staging_with_registry_cache(
    project_start: &Path,
    registry_cache_root: Option<&Path>,
    options: RunOptions,
    build: StagingBuild,
    ui: &dyn crate::Reporter,
) -> Result<ResolvedStagingInput> {
    crate::progress::ensure_active(ui)?;
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let robot = load_robot(&robot_path)?;

    // The driver policy is resolved from the parsed robot BEFORE resolution
    // (#936, finding 2): it must gate resolution itself, so an excluded driver
    // is never resolved, materialized, staged, required, inspected, or planned.
    let driver_policy = DriverPolicy::from_options(&options, &driven_instances(&robot))?;

    // A cross `--target` resolves official packages for that target (the same
    // per-target resolution `phoxal build --target` performs); a host pass
    // leaves both targets unset so resolution uses the host triple.
    let official_target = build.target().map(str::to_string);
    let resolved = crate::progress::run_phase(
        ui,
        crate::PhaseId::new("validate"),
        "Validating robot.yaml",
        || {
            let options = ResolveOptions {
                official_target_triple: official_target.clone(),
                drivers: driver_policy.selection(),
                include_simulators: build.include_simulators(),
                offline: options.offline,
            };
            if let Some(registry_cache_root) = registry_cache_root {
                crate::resolve::project::resolve_with_train_using_registry_cache(
                    &robot,
                    project_root,
                    registry_cache_root,
                    options,
                    |train| {
                        ui.report(crate::PreparationEvent::ProjectResolved {
                            train: train.to_string(),
                        });
                    },
                )
            } else {
                resolve_with_train(&robot, project_root, options, |train| {
                    ui.report(crate::PreparationEvent::ProjectResolved {
                        train: train.to_string(),
                    });
                })
            }
        },
    )?;

    Ok(ResolvedStagingInput {
        project_root: project_root.to_path_buf(),
        resolved,
        driver_policy,
        options,
        build,
    })
}

/// Resolve a buildable source project once, then materialize and validate that
/// exact result through [`refresh_staging_resolved`].
pub(crate) fn refresh_staging(
    project_start: &Path,
    options: &RunOptions,
    build: &StagingBuild,
    check_source: bool,
    run: RunIdentity,
    ui: &dyn crate::Reporter,
) -> Result<StagedProject> {
    let input = resolve_staging(project_start, options.clone(), build.clone(), ui)?;
    refresh_staging_resolved(input, check_source, run, ui)
}

/// Materialize one resolved source graph, validate the whole compiled layout
/// through the loader, and publish only a complete candidate.
///
/// `run`, `start`, and every local/container build backend converge here after
/// resolution. All materialization, source validation, flat `bin/` completion,
/// and loader validation happens against an unpublished candidate exactly once;
/// only then is it published as `.phoxal/bundle/` (organization#951 WS4).
/// A failure anywhere therefore leaves the previous live bundle untouched.
pub(crate) fn refresh_staging_resolved(
    input: ResolvedStagingInput,
    check_source: bool,
    run: RunIdentity,
    ui: &dyn crate::Reporter,
) -> Result<StagedProject> {
    crate::progress::ensure_active(ui)?;
    let ResolvedStagingInput {
        project_root,
        resolved,
        driver_policy,
        options,
        build,
    } = input;
    // Stage into an UNPUBLISHED candidate. Every install, source build,
    // metadata read, and loader validation below runs against
    // `candidate.path()`; only the final `publish_runtime_layout` call at the
    // bottom of this function ever touches the live `.phoxal/bundle/`.
    // Driver exclusion is applied HERE, at finalization, before any binary is
    // resolved or inspected: the excluded `driver:` blocks are stripped out of
    // the finalized document, so nothing downstream can see them.
    let intent = phoxal_cli_core::project::intent::RunIntent::real(driver_policy.selection());
    let candidate = crate::stage::begin_runtime_layout(&project_root, &resolved, &intent)
        .context("failed to stage the finalized bundle")?;
    crate::progress::ensure_active(ui)?;
    let materialize_settings = build.materialize_settings(&project_root, options.offline)?;

    let source_participants = source_participants_from_resolved(&project_root, &resolved)?;
    // Driver selection is resolution-visible: an excluded source driver must
    // be absent from the Cargo command as well as the staged layout. Keep the
    // full list for source cwd provenance, but plan only selected artifacts.
    let selected_source_participants =
        selected_native_source_participants(&source_participants, driver_policy.selection());

    // Compile the complete selected source subset before either checking or
    // staging sees it. Cargo's JSON artifact path is authoritative.
    let source_artifacts = build_selected_source_artifacts(
        &selected_source_participants,
        build.target(),
        build.source_profile(),
        build.prebuilt_target_dir(),
        options.offline,
        ui,
    )?;

    // Registry entries remain Cargo-install-owned. Source overrides consume
    // the already-built bytes rather than opening another Cargo invocation.
    let extra_registry_runtimes = resolved
        .components
        .iter()
        .filter(|component| {
            driver_policy
                .selection()
                .includes_instance(&component.instance)
        })
        .filter_map(|component| component.driver.as_ref())
        .filter_map(|driver| driver.registry_runtime())
        .collect::<Vec<_>>();
    crate::stage::materialize_candidate_store(
        candidate.path(),
        &resolved,
        &extra_registry_runtimes,
        options.offline,
        build.officials_source(),
        &materialize_settings,
        ui,
        |_crate_dir, name| source_artifacts.binary_named(name).map(PathBuf::from),
    )
    .context("failed to materialize official runtimes")?;
    crate::progress::ensure_active(ui)?;

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
        crate::progress::run_phase(
            ui,
            crate::PhaseId::new("check"),
            "Checking source graph",
            || {
                run_source_check(
                    candidate.path(),
                    &resolved.source_manifest,
                    &resolved,
                    &selected_source_participants,
                    &source_artifacts,
                    driver_policy.selection(),
                    ui,
                )
            },
        )?;
    }

    // Complete the candidate `bin/` store so the loader can inspect every
    // required runtime off-disk. This is the last step that consumes the
    // resolved graph; everything after it reads only the candidate layout.
    stage_complete_bin_store(
        candidate.path(),
        &selected_source_participants,
        &source_artifacts,
    )?;
    crate::progress::ensure_active(ui)?;

    // Declaration drift (#950) is warned from THIS shared path, so run,
    // start and build all surface it exactly once.
    report_undeclared_runtimes(&resolved.undeclared_runtimes, ui);

    // The loader's own execution-time validation - config-schema pairing and
    // architecture inspection - runs against the candidate too, still before
    // publish. `run`/`start` inspect against the host; a `--target` build
    // inspects against the declared target signature instead, since the
    // staged binaries were cross-compiled (or container-built) for it, not
    // for this host.
    let inspection = match build.target() {
        Some(target) => {
            LayoutInspection::Target(expected_target_for_triple(target).with_context(|| {
                format!("cannot validate the staged runtime layout for target {target}")
            })?)
        }
        None => LayoutInspection::Host,
    };
    let mut plan = crate::load::layout::validate_layout_plan(candidate.path(), inspection, run)
        .context("failed to validate the finalized bundle candidate")?;
    crate::progress::ensure_active(ui)?;

    // Every install, build, check, and validation above succeeded against the
    // candidate alone - publish it as the live layout now, and only now.
    let candidate_path = candidate.path().to_path_buf();
    let staged_root = crate::progress::run_phase(
        ui,
        crate::PhaseId::new("publish"),
        "Publishing runtime layout",
        || {
            crate::stage::publish_runtime_layout(candidate, &resolved)
                .context("failed to publish the staged runtime layout")
        },
    )?;

    // The plan above was constructed and validated against the unpublished
    // candidate - `validate_layout_plan` opened the layout at
    // `candidate.path()`, so every participant's `ParticipantLaunch::bundle_root`
    // (the `--bundle-root` every launched process receives) was baked in as the
    // candidate path. The rename just above made `staged_root` the live layout
    // and the candidate path no longer exists, so every one of those recorded
    // roots is now dangling. Repoint them to the published root - the same fix
    // `simulate` already applies to its specs' `executable`/`cwd`
    // (`repoint_after_publish`), for the identical reason: both stage against a
    // path that publish then renames away.
    repoint_plan_bundle_roots(&mut plan, &candidate_path, &staged_root);

    Ok(StagedProject {
        project_root,
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
pub(crate) fn prepare_source_run(
    project_start: &Path,
    options: RunOptions,
    ui: &dyn crate::Reporter,
    run: RunIdentity,
) -> Result<PreparedRun> {
    let staged = refresh_staging(
        project_start,
        &options,
        &StagingBuild::host_runtime(),
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

    // Explain any policy-excluded drivers as a session-level advisory: they are
    // never plan participants, so this summary is the only signal an operator
    // gets for why hardware rows are absent (#936, finding 8).
    report_excluded_drivers(
        &staged.driver_policy,
        &driven_instances(&staged.resolved.source_manifest),
        ui,
    );

    // The staging-side record of source crate directories the source-free plan
    // no longer carries: a participant built from local source runs from its
    // crate directory (relative asset resolution) and is rebuilt there under
    // Execution identity always comes from the plan's `bin/` name.
    let source_dirs = source_dirs_by_participant(&staged.source_participants);
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
        source_cwd(participant, &staged.resolved, &source_dirs)
    };
    let participants = build_layout_specs(&plan, &layout, &cwd_for)?;

    // Resolve the router config from the STAGED layout, not the source tree:
    // staging copies `router.config` into the layout under its relative path, so
    // a source run and an extracted `build.phoxal` resolve the identical staged
    // asset (#936, finding 4).
    let router_config = resolve_layout_router_config(&staged.staged_root)?;
    let StagedProject {
        project_root,
        resolved,
        staged_root,
        ..
    } = staged;

    Ok(PreparedRun {
        project_root,
        train: resolved.train,
        plan,
        participants,
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
pub(crate) fn prepare_layout_run(
    layout_root: &Path,
    options: RunOptions,
    run: RunIdentity,
    reporter: &dyn crate::Reporter,
) -> Result<PreparedRun> {
    let layout = RuntimeLayout::open(layout_root)
        .with_context(|| format!("failed to open finalized bundle {}", layout_root.display()))?;

    // Driver selection was applied at finalization, by stripping the excluded
    // `driver:` blocks out of this bundle's own document. There is nothing left
    // to select here, so a driver flag against an existing bundle is refused
    // rather than silently ignored.
    anyhow::ensure!(
        matches!(options.drivers, crate::DriverMode::On) && options.drivers_subset.is_empty(),
        "driver selection is written into the bundle at build time; run the source project to \
         change it, or run this bundle as it was finalized"
    );
    let plan = crate::progress::run_phase(
        reporter,
        crate::PhaseId::new("validate"),
        "Opening staged layout",
        || {
            crate::load::layout::validate_layout_plan(layout_root, LayoutInspection::Host, run)
                .context("failed to construct the launch plan from the finalized bundle")
        },
    )?;
    reporter.report(crate::PreparationEvent::ProjectResolved {
        train: "staged".to_string(),
    });
    // An extracted bundle / staged layout has no source, so no participant has a
    // crate cwd - the closure always yields `None` (#936, finding 3).
    let participants = build_layout_specs(&plan, &layout, &|_| None)?;

    let router_config = resolve_layout_router_config(layout_root)?;

    Ok(PreparedRun {
        project_root: layout_root.to_path_buf(),
        train: "staged".to_string(),
        plan,
        participants,
        staged_root: layout_root.to_path_buf(),
        router_config,
    })
}

pub(crate) fn prepare_run(request: PrepareRunRequest) -> Result<PreparedExecution> {
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let options = RunOptions {
        drivers: request.drivers.mode,
        drivers_subset: request.drivers.subset,
        offline: request.offline,
    };
    let execution_root =
        crate::paths::runtime::pin_installed_release(&request.target.logical_root)?;
    let prepared = match classify_run_root(&execution_root)? {
        RunRootKind::Source => prepare_source_run(
            &execution_root,
            options,
            request.reporter.as_ref(),
            request.run,
        )?,
        RunRootKind::Layout => prepare_layout_run(
            &execution_root,
            options,
            request.run,
            request.reporter.as_ref(),
        )?,
    };
    let router = PreparedRouter {
        config: prepared.router_config,
        endpoint: request.target.zenoh_endpoint.clone(),
    };
    let mut plan = prepared.plan;
    let mut participants = prepared.participants;
    apply_session_connect(&mut plan, &mut participants, &router.endpoint);
    Ok(PreparedExecution {
        target: request.target,
        project_root: prepared.project_root,
        manual_input: crate::manual_input_from_staged_root(&prepared.staged_root),
        staged_root: prepared.staged_root,
        train: prepared.train,
        plan,
        participants,
        router,
        simulation: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunRootKind {
    Source,
    Layout,
}

/// Classify the root from its filesystem shape only. In particular, do not use
/// locked-train resolution as a probe: a malformed or missing lock is a source
/// project validation error, not evidence that the root is neither source nor
/// layout.
fn classify_run_root(root: &Path) -> Result<RunRootKind> {
    let has_robot = root.join("robot.yaml").is_file();
    if has_robot && root.join("Cargo.toml").is_file() {
        return Ok(RunRootKind::Source);
    }
    if RuntimeLayout::is_layout_root(root) {
        return Ok(RunRootKind::Layout);
    }
    if has_robot {
        return Ok(RunRootKind::Source);
    }
    anyhow::bail!(
        "{} is neither a buildable source project (no robot.yaml/root Cargo package) nor a \
         staged runtime layout (no robot.json/assets next to bin/); run from a robot project or extract \
         a build.phoxal bundle first",
        root.display()
    );
}

pub(crate) fn apply_session_connect(
    plan: &mut LaunchPlan,
    participants: &mut [crate::PreparedParticipant],
    endpoint: &str,
) {
    for robot in &mut plan.robots {
        for participant in &mut robot.participants {
            participant.launch.bus.connect_endpoints = vec![endpoint.to_string()];
        }
    }
    for launch in participants
        .iter_mut()
        .filter_map(|participant| participant.launch.as_mut())
    {
        if let Some((_, value)) = launch
            .env
            .iter_mut()
            .find(|(key, _)| key == phoxal_runtime_contract::env::CONNECT)
        {
            *value = endpoint.to_string();
        }
    }
}

pub(crate) fn resolve_layout_router_config(root: &Path) -> Result<Option<PathBuf>> {
    let path = root.join(phoxal_cli_core::project::layout::ROUTER_CONFIG_PATH);
    Ok(path.is_file().then_some(path))
}

/// Repoint every participant's `bundle_root` from the unpublished candidate to
/// the published layout, in place. `construct_plan`/`construct_plan_from_selected`
/// (`crates/core/src/project/layout/plan.rs`) set `bundle_root` to the exact
/// root the plan was constructed against - the candidate, here - so every
/// participant carries it, never only some.
pub(crate) fn repoint_plan_bundle_roots(plan: &mut LaunchPlan, candidate: &Path, published: &Path) {
    for robot in &mut plan.robots {
        for participant in &mut robot.participants {
            if let Some(bundle_root) = participant.launch.bundle_root.as_mut() {
                repoint_after_publish(bundle_root, candidate, published);
            }
        }
    }
}

/// Rewrite `path` from the candidate root to the published root when it falls
/// under the candidate at all - a source participant's `cwd` is its own crate
/// directory, never under either, and is correctly left untouched. `fs::rename`
/// (the publish step) preserves the relative structure exactly, so this prefix
/// swap is exact, never an approximation. Shared by `run`
/// ([`repoint_plan_bundle_roots`], above) and `simulate`
/// (`simulation::setup::live_simulate_setup`, which repoints each spec's
/// `executable`/`cwd`) - both stage against an unpublished candidate and must
/// repoint every candidate-derived path once publish renames it away.
pub(crate) fn repoint_after_publish(path: &mut PathBuf, candidate: &Path, published: &Path) {
    if let Ok(relative) = path.strip_prefix(candidate) {
        *path = published.join(relative);
    }
}

#[cfg(test)]
mod root_classification_tests {
    use super::*;

    const LAYOUT_YAML: &str = r#"schema: phoxal/robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators:
      - wheel.motor
    encoders: []
  components:
    wheel:
      component: wheel
      mount_link: base
"#;

    #[test]
    fn classifies_source_layout_and_neither_without_resolving_dependencies() -> Result<()> {
        let source = tempfile::tempdir()?;
        std::fs::write(
            source.path().join("robot.yaml"),
            "schema: phoxal/robot/v0\n",
        )?;
        assert_eq!(classify_run_root(source.path())?, RunRootKind::Source);

        let layout = tempfile::tempdir()?;
        crate::stage::write_test_bundle(
            layout.path(),
            LAYOUT_YAML,
            &phoxal_cli_core::project::intent::RunIntent::default(),
            &[],
        )?;
        assert_eq!(classify_run_root(layout.path())?, RunRootKind::Layout);

        let source_with_bin = tempfile::tempdir()?;
        std::fs::write(
            source_with_bin.path().join("robot.yaml"),
            "schema: phoxal/robot/v0\n",
        )?;
        std::fs::write(source_with_bin.path().join("Cargo.toml"), "[workspace]\n")?;
        std::fs::create_dir(source_with_bin.path().join("bin"))?;
        assert_eq!(
            classify_run_root(source_with_bin.path())?,
            RunRootKind::Source
        );

        let neither = tempfile::tempdir()?;
        let error = classify_run_root(neither.path()).unwrap_err();
        assert!(format!("{error:#}").contains("is neither a buildable source project"));
        Ok(())
    }
}

/// The source-time graph check: build every source participant's binary for its
/// embedded metadata and validate the source graph, failing the run if the
/// train's check gate rejects it. This is a staging-side gate; the loader
/// re-validates config over the staged layout.
fn run_source_check(
    staged_root: &Path,
    robot: &phoxal_manifest::source::robot::v0::Manifest,
    resolved: &BundlePlan,
    source_participants: &[phoxal_cli_core::check::source::SourceParticipant],
    source_artifacts: &SourceArtifacts,
    drivers: phoxal_cli_core::project::intent::DriverSelection,
    reporter: &dyn crate::Reporter,
) -> Result<()> {
    let bin_dir = staged_root.join("bin");
    let platform_refs = check_artifact_refs_from_resolved(resolved, drivers);
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
    official_by_name.extend(crate::validation::component_driver_runtimes_by_ref(
        resolved,
    ));
    let outcome = run_check_with_context(
        &platform_refs,
        source_participants,
        CheckGraphContext { robot: Some(robot) },
        |binary_name| {
            if let Some(runtime) = official_by_name.get(binary_name) {
                return extract_participant_report_from_staged_runtime(&bin_dir, runtime);
            }
            Err(anyhow!(
                "resolved official artifact {binary_name} was not materialized into bin/"
            ))
        },
        |participant| {
            build_participant_report_from_binary(
                participant,
                source_artifacts.binary(participant)?,
                reporter,
            )
        },
    )?;
    if !outcome.is_ok() {
        crate::validation::ensure_check_outcome_ok(&outcome)?;
    }
    Ok(())
}

fn selected_native_source_participants(
    participants: &[phoxal_cli_core::check::source::SourceParticipant],
    drivers: phoxal_cli_core::project::intent::DriverSelection,
) -> Vec<phoxal_cli_core::check::source::SourceParticipant> {
    use phoxal_cli_core::check::source::SourceParticipantKind;
    participants
        .iter()
        .filter(|participant| match participant.kind {
            // A simulator override belongs only to the Webots command.
            SourceParticipantKind::Simulator => false,
            SourceParticipantKind::ComponentDriver => drivers.includes_instance(&participant.name),
            _ => true,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::check::participant_metadata::{host_architecture, host_binary_format};
    use phoxal_cli_core::project::intent::{DriverSelection, RunIntent};
    use phoxal_cli_core::project::requirements::RequiredParticipantKind;

    const ROBOT_YAML: &str = r#"schema: phoxal/robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators:
      - wheel.motor
    encoders: []
  components:
    wheel:
      component: wheel
      mount_link: base
"#;

    /// Synthesize a host-format object carrying the phoxal metadata section a
    /// required runtime's own identity must match, so
    /// `RuntimeLayout::construct_plan` can inspect a real object shape off-disk
    /// with no actual binary built (mirrors `run::participants`' own fixture).
    fn synthesize_binary_with_id(id: &str, kind: &str) -> Vec<u8> {
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
        let payload =
            crate::stage::test_metadata_payload(id, kind, serde_json::json!({"type": "null"}));
        obj.append_section_data(section, &payload, 1);
        obj.write().expect("synthesize object file")
    }

    #[test]
    fn native_source_selection_excludes_simulators_and_unselected_drivers() {
        use phoxal_cli_core::check::source::SourceParticipant;

        let participants = vec![
            SourceParticipant::user_service("mission", PathBuf::from("runtimes/mission")),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive",
                "ddsm115",
                PathBuf::from("components/ddsm115"),
            ),
            SourceParticipant::simulator(
                "webots-controller",
                "webots-controller",
                PathBuf::from("simulators/webots-controller"),
            ),
        ];

        let none = selected_native_source_participants(&participants, DriverSelection::None);
        assert_eq!(
            none.iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["mission"]
        );
        let only_left = selected_native_source_participants(
            &participants,
            DriverSelection::Only(["left_drive".to_string()].into_iter().collect()),
        );
        assert_eq!(
            only_left
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["mission", "left_drive"]
        );
    }

    /// Stage a minimal but complete layout - canonical `robot.json` plus a
    /// synthesized binary for every runtime the loader requires - directly at
    /// `root`, mirroring what `stager::begin_runtime_layout` +
    /// `stage_complete_bin_store` leave behind in a real candidate directory,
    /// with no Cargo or network involved.
    fn stage_layout(root: &Path) -> Result<()> {
        crate::stage::write_test_bundle(root, ROBOT_YAML, &RunIntent::default(), &[])?;
        let bin = root.join("bin");
        let layout = RuntimeLayout::open(root)?;
        for (binary_name, required) in layout.requirements().selected_binaries() {
            std::fs::write(
                bin.join(binary_name),
                synthesize_binary_with_id(
                    &required.artifact_id,
                    match required.kind {
                        RequiredParticipantKind::Brain => "brain",
                        RequiredParticipantKind::OfficialService
                        | RequiredParticipantKind::UserService => "service",
                        RequiredParticipantKind::ComponentDriver => "driver",
                        RequiredParticipantKind::WorldClock => "simulator",
                    },
                ),
            )?;
        }
        Ok(())
    }

    /// The bug this module fixes: `refresh_staging` constructs and validates
    /// the launch plan against the unpublished candidate
    /// (`crate::load::layout::validate_layout_plan(candidate.path(), ...)`), which
    /// bakes the candidate root into every participant's
    /// `ParticipantLaunch::bundle_root` (`crates/core/src/project/layout/plan.rs`,
    /// `construct_plan_from_selected`: `bundle_root = self.root().to_path_buf()`).
    /// `publish_runtime_layout` then renames that candidate away, so every one
    /// of those recorded roots goes stale - the real symptom was every launched
    /// participant's `--bundle-root` naming a `.bundle-candidate-*` directory
    /// that no longer existed. This fails if `repoint_plan_bundle_roots` is not
    /// called, or is called with the wrong `(candidate, published)` pair.
    #[test]
    fn repoint_plan_bundle_roots_leaves_no_participant_pointing_at_the_candidate() -> Result<()> {
        let project = tempfile::tempdir()?;
        // Named like a real staging candidate (`stager::begin_runtime_layout`
        // prefixes with `.bundle-candidate-`); the repoint itself only cares
        // about the prefix match, not the name.
        let candidate = project.path().join(".phoxal/.bundle-candidate-test0000");
        stage_layout(&candidate)?;

        let mut plan = RuntimeLayout::construct_plan(&candidate, RunIdentity::default())?.plan;
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
                participant.launch.bundle_root.as_deref(),
                Some(candidate.as_path()),
                "{} must start out pointing at the candidate",
                participant.launch.participant_id
            );
        }

        let published = project.path().join(".phoxal/bundle");
        repoint_plan_bundle_roots(&mut plan, &candidate, &published);

        for participant in plan.robots.iter().flat_map(|robot| &robot.participants) {
            let bundle_root = participant.launch.bundle_root.as_ref().unwrap_or_else(|| {
                panic!("{} lost its bundle_root", participant.launch.participant_id)
            });
            assert!(
                !bundle_root.starts_with(&candidate),
                "{} still references the unpublished candidate: {}",
                participant.launch.participant_id,
                bundle_root.display()
            );
            assert_eq!(
                bundle_root, &published,
                "{} did not repoint to the published layout",
                participant.launch.participant_id
            );
        }
        Ok(())
    }
}
