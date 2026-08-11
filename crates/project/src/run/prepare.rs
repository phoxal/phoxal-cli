//! Prepare responsibilities for run.
//!
//! `run` is universal (): it prepares the same [`PreparedRun`] whether the
//! root is a buildable source project or an already-staged runtime layout (an
//! extracted `build.phoxal` or a `.phoxal/bundle/` directory). Both end
//! at one execution artifact: a verified [`phoxal_bundle::RuntimeBundle`]. The
//! two entry points differ only in the staging step before it: a source root
//! resolves, checks, and stages; a bundle root has nothing to build and runs in
//! place.

use super::{DriverPolicy, RunOptions};
use super::{PrepareRunRequest, PreparedExecution};
use crate::build::cargo::{SourceArtifacts, build_selected_source_artifacts};
use crate::build::profile::StagingBuild;
use crate::resolve::project::resolve_with_train;
use crate::run::participants::stage_complete_bin_store;
use crate::run::report::{driven_instances, report_excluded_drivers, report_undeclared_runtimes};
use crate::source::resolver::BundlePlan;
use crate::source::resolver::ResolveOptions;
use crate::source::resolver::discover_robot_yaml;
use crate::source::resolver::load_robot;
use crate::validation::CheckGraphContext;
use crate::validation::build_participant_report_from_binary;
use crate::validation::check_artifact_refs_from_resolved;
use crate::validation::extract_participant_report_from_staged_runtime;
use crate::validation::run_check_with_context;
use crate::validation::source_participants_from_resolved;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
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
/// constructed against `staged_root` before it was published; callers must reuse it rather than re-running
/// `loader::validate_layout_plan`, which would be a second, redundant
/// validation pass over already-validated bytes.
pub(crate) struct StagedProject {
    pub(crate) resolved: BundlePlan,
    pub(crate) driver_policy: DriverPolicy,
    /// The staged runtime layout root - `.phoxal/bundle/`.
    pub(crate) staged_root: std::path::PathBuf,
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
    // and must gate resolution itself, so an excluded driver
    // is never resolved, materialized, staged, required, inspected, or planned.
    let driver_policy = DriverPolicy::from_options(&options, &driven_instances(&robot))?;

    // A cross `--target` resolves official packages for that target (the same
    // per-target resolution `phoxal build --target` performs); a host pass
    // leaves both targets unset so resolution uses the host triple.
    let official_target = build.target().map(str::to_string);
    let resolved = crate::progress::run_phase(
        ui,
        crate::progress_phase::PhaseId::new("validate"),
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
    ui: &dyn crate::Reporter,
) -> Result<StagedProject> {
    let input = resolve_staging(project_start, options.clone(), build.clone(), ui)?;
    refresh_staging_resolved(input, check_source, ui)
}

/// Materialize one resolved source graph, validate the whole compiled layout
/// through the loader, and publish only a complete candidate.
///
/// `run`, `start`, and every local/container build backend converge here after
/// resolution. All materialization, source validation, flat `bin/` completion,
/// and loader validation happens against an unpublished candidate exactly once;
/// only then is it published as `.phoxal/bundle/`.
/// A failure anywhere therefore leaves the previous live bundle untouched.
pub(crate) fn refresh_staging_resolved(
    input: ResolvedStagingInput,
    check_source: bool,
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
    let candidate = crate::stage::begin_runtime_layout(&project_root, &resolved)
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
    // (cross-built) binaries is the authoritative check for a bundle ().
    if check_source {
        crate::progress::run_phase(
            ui,
            crate::progress_phase::PhaseId::new("check"),
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
    crate::stage::write_runtime_document(candidate.path(), &resolved)?;
    crate::progress::ensure_active(ui)?;

    // Declaration drift () is warned from THIS shared path, so run,
    // start and build all surface it exactly once.
    report_undeclared_runtimes(&resolved.undeclared_runtimes, ui);

    // The loader's own execution-time validation - config-schema pairing and
    // architecture inspection - runs against the candidate too, still before
    // publish. `run`/`start` inspect against the host; a `--target` build
    // inspects against the declared target signature instead, since the
    // staged binaries were cross-compiled (or container-built) for it, not
    // for this host.
    // Every install, build, check, and validation above succeeded against the
    // candidate alone - publish it as the live layout now, and only now.
    let staged_root = crate::progress::run_phase(
        ui,
        crate::progress_phase::PhaseId::new("publish"),
        "Publishing runtime layout",
        || {
            crate::stage::publish_runtime_layout(candidate)
                .context("failed to publish the staged runtime layout")
        },
    )?;

    Ok(StagedProject {
        resolved,
        driver_policy,
        staged_root,
    })
}

/// Prepare a run from a buildable source project: refresh the staged runtime
/// layout through the shared [`refresh_staging`] entry, then construct and
/// validate the launch plan from that staged layout. The plan and every
/// executable come from the staged layout, never the resolved graph directly
/// () - the resolved graph is a staging-side input only.
pub(crate) fn prepare_source_run(
    project_start: &Path,
    options: RunOptions,
    ui: &dyn crate::Reporter,
) -> Result<PathBuf> {
    let staged = refresh_staging(
        project_start,
        &options,
        &StagingBuild::host_runtime(),
        true,
        ui,
    )?;

    // Explain any policy-excluded drivers as a session-level advisory: they are
    // never plan participants, so this summary is the only signal an operator
    // gets for why hardware rows are absent.
    report_excluded_drivers(
        &staged.driver_policy,
        &driven_instances(&staged.resolved.source_manifest),
        ui,
    );

    // The staging-side record of source crate directories the source-free plan
    // no longer carries: a participant built from local source runs from its
    // crate directory (relative asset resolution) and is rebuilt there under
    // Execution identity always comes from the plan's `bin/` name.
    Ok(staged.staged_root)
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
    reporter: &dyn crate::Reporter,
) -> Result<PathBuf> {
    // Driver selection was applied at finalization, by stripping the excluded
    // `driver:` blocks out of this bundle's own document. There is nothing left
    // to select here, so a driver flag against an existing bundle is refused
    // rather than silently ignored.
    anyhow::ensure!(
        matches!(options.drivers, super::DriverMode::On) && options.drivers_subset.is_empty(),
        "driver selection is written into the bundle at build time; run the source project to \
         change it, or run this bundle as it was finalized"
    );
    crate::progress::run_phase(
        reporter,
        crate::progress_phase::PhaseId::new("validate"),
        "Opening runtime bundle",
        || {
            phoxal_bundle::RuntimeBundle::open_verified(layout_root)
                .context("failed to verify the compiled runtime bundle")
        },
    )?;
    reporter.report(crate::PreparationEvent::ProjectResolved {
        train: "staged".to_string(),
    });
    Ok(layout_root.to_path_buf())
}

pub fn prepare_run(request: PrepareRunRequest) -> Result<PreparedExecution> {
    crate::progress::ensure_active(request.reporter.as_ref())?;
    let options = RunOptions {
        drivers: request.drivers.mode,
        drivers_subset: request.drivers.subset,
        offline: request.offline,
    };
    let execution_root =
        crate::paths::runtime::pin_installed_release(&request.target.logical_root)?;
    let prepared = match classify_run_root(&execution_root)? {
        RunRootKind::Source => {
            prepare_source_run(&execution_root, options, request.reporter.as_ref())?
        }
        RunRootKind::Layout => {
            prepare_layout_run(&execution_root, options, request.reporter.as_ref())?
        }
    };
    Ok(PreparedExecution {
        staged_root: prepared,
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
    if root.join(phoxal_bundle::RUNTIME_FILE).is_file() {
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

/// The source-time graph check: build every source participant's binary for its
/// embedded metadata and validate the source graph, failing the run if the
/// train's check gate rejects it. This is a staging-side gate; the loader
/// re-validates config over the staged layout.
fn run_source_check(
    staged_root: &Path,
    robot: &phoxal_manifest::source::robot::v0::Manifest,
    resolved: &BundlePlan,
    source_participants: &[crate::check::source::SourceParticipant],
    source_artifacts: &SourceArtifacts,
    drivers: crate::source::intent::DriverSelection,
    reporter: &dyn crate::Reporter,
) -> Result<()> {
    let bin_dir = staged_root.join("bin");
    let project_framework = resolved.train.framework();
    let platform_refs = check_artifact_refs_from_resolved(resolved, drivers);
    let mut official_by_name = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| {
            (
                crate::source::resolver::official_binary_name(runtime.kind, &runtime.name),
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
                return extract_participant_report_from_staged_runtime(
                    &bin_dir,
                    runtime,
                    project_framework,
                );
            }
            Err(anyhow!(
                "resolved official artifact {binary_name} was not materialized into bin/"
            ))
        },
        |participant| {
            build_participant_report_from_binary(
                participant,
                source_artifacts.binary(participant)?,
                project_framework,
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
    participants: &[crate::check::source::SourceParticipant],
    drivers: crate::source::intent::DriverSelection,
) -> Vec<crate::check::source::SourceParticipant> {
    use crate::check::source::SourceParticipantKind;
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
