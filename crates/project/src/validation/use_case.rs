use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::check::source::{SourceParticipant, SourceParticipantKind};
use crate::source::resolver::{BundlePlan, ResolvedComponentDriver};
use crate::source::train::LockedProject;
use anyhow::{Context, Result, anyhow};
use phoxal::authoring::source::robot::v0::Manifest as Robot;
use phoxal::version::FrameworkVersion;
use phoxal_cli_catalog::{ArtifactKind, Catalog};

use super::{
    ValidateRequest, ValidationComponent, ValidationReport, ValidationService, ValidationSource,
};
use crate::validation::{
    CheckGraphContext, CheckOutcome, PlatformArtifactRef, RawParticipantReport,
    build_participant_report_from_binary, component_driver_platform_refs_from_resolved,
    component_driver_runtimes_by_ref, ensure_check_outcome_ok,
    extract_participant_report_from_staged_runtime, platform_artifact_refs_from_resolved,
    platform_runtimes_by_ref, run_check_with_context,
};

pub fn validate(request: ValidateRequest) -> Result<ValidationReport> {
    let project_start = match &request.source {
        ValidationSource::Project(project) => project,
        ValidationSource::Archive(archive) => {
            crate::bundle::archive::extract_build_archive(&archive.archive, &archive.destination)?;
            // A build archive is a whole deployment release. Its shape is
            // proven here, supervisor beside bundle, so an installer never
            // activates a release that is missing the supervisor that runs it -
            // including an archive built before releases owned their supervisor.
            let release = crate::deployment::validate_release(&archive.destination)
                .context("build archive is not a valid deployment release")?;
            let expected = crate::check::participant_metadata::expected_target_for_host();
            crate::check::participant_metadata::ensure_target(
                &std::fs::read(&release.supervisor).with_context(|| {
                    format!("failed to read supervisor {}", release.supervisor.display())
                })?,
                &release.supervisor.display().to_string(),
                &expected,
            )
            .context("this release's supervisor cannot run on this host")?;
            let bundle = crate::load::layout::validate_runtime_bundle(&release.bundle, expected)
                .context("runtime archive failed verification")?;
            return Ok(ValidationReport {
                robot_path: release.bundle.join(phoxal::bundle::MANIFEST_FILE),
                robot: bundle.robot_id().to_string(),
                train: String::new(),
                platform_services: Vec::new(),
                services: Vec::new(),
                components: Vec::new(),
            });
        }
    };
    let loaded = crate::load::project::load(project_start)?;
    let project_root = loaded.root.as_path();
    let robot_path = loaded.robot_path;
    let robot = loaded.robot;
    crate::progress::run_phase(
        request.reporter.as_ref(),
        crate::progress_phase::PhaseId::new("validate"),
        "Validating robot.yaml",
        || {
            robot
                .validate()
                .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))
        },
    )?;

    // Resolve the locked workspace once and share that result with canonical
    // compilation and config checking. Resolving component asset roots may
    // fetch packages from the Phoxal registry unless `--offline` was selected.
    let project = crate::source::train::resolve_locked_project(
            project_root,
            request.offline,
        )
        .map_err(|error| {
            anyhow!(
                "Cargo workspace runtime discovery failed:\nlocked Cargo workspace is invalid: {error:#}"
            )
        })?;
    let resolved = crate::resolve::project::resolve_with_locked_project(
        &robot,
        project_root,
        crate::source::resolver::ResolveOptions {
            offline: request.offline,
            ..Default::default()
        },
        &project,
    )?;
    let project_framework = resolved.train.framework();
    let train = resolved.train.version().to_string();
    let workspace = workspace_runtime_report(&robot, &project);
    for success in &workspace.successes {
        request.reporter.success(success.clone());
    }
    anyhow::ensure!(
        workspace.problems.is_empty(),
        "Cargo workspace runtime discovery failed:\n{}",
        workspace.problems.join("\n")
    );

    // Every authored configuration must satisfy the JSON Schema the binary
    // that reads it embeds: every `services.<id>.config` the document declares
    // - this project's own service or an official one it configures - against
    // that service's `#[phoxal::service(config = ...)]` type, and a driven
    // instance's `driver:` block against its driver's config type and declared
    // connection kind. There is no schema until those types compile, so this
    // is the one part of `validate` that is not free - it builds the root
    // brain and ONLY the declared source crates (never the official set, never
    // a staged bundle), through the same check engine `build`/`run`/`simulate`
    // already use.
    let source_participants = declared_source_participants(&robot, &project, &resolved);
    request.reporter.info(compile_notice(&source_participants));
    let artifacts = crate::build::cargo::build_selected_source_artifacts(
        &source_participants,
        None,
        crate::build::profile::Profile::Debug,
        None,
        request.offline,
        request.reporter.as_ref(),
    )?;
    // A registry artifact's contract lives in a binary this command may not
    // materialize, so it is read from the project's own staged release when
    // one is there and reported unchecked when it is not.
    let staged = staged_registry_reports(
        project_root,
        &robot,
        &resolved,
        project_framework,
        request.reporter.as_ref(),
    );
    let outcome = check_declared_sources(
        &robot,
        &staged.refs,
        &source_participants,
        |binary_name| {
            staged.reports.get(binary_name).cloned().ok_or_else(|| {
                anyhow!(
                    "validate reads only already-staged binaries (unexpected request for \
                     {binary_name})"
                )
            })
        },
        |participant| {
            build_participant_report_from_binary(
                participant,
                artifacts.binary(participant)?,
                project_framework,
                request.reporter.as_ref(),
            )
        },
    )?;
    ensure_check_outcome_ok(&outcome)?;
    let mut checked_instances = BTreeSet::new();
    let mut checked_services = BTreeSet::new();
    for participant in &outcome.checked_participants {
        match &participant.scope {
            crate::check::ParticipantScope::ComponentInstance(instance) => {
                checked_instances.insert(instance.clone());
            }
            crate::check::ParticipantScope::Graph => {
                if participant.participant_kind == crate::check::ParticipantKind::Service {
                    checked_services.insert(participant.participant_id.clone());
                }
            }
        }
    }
    let catalog = Catalog::official();
    Ok(ValidationReport {
        robot_path,
        robot: robot.robot.id.clone(),
        train,
        platform_services: catalog
            .native()
            .filter(|official| official.kind == ArtifactKind::Service)
            .map(|official| official.package.to_string())
            .collect(),
        services: robot
            .services
            .keys()
            .map(|id| ValidationService {
                id: id.clone(),
                official: catalog.is_official_service(id),
                config_checked: checked_services.contains(id),
            })
            .collect(),
        components: robot
            .robot
            .components
            .iter()
            .map(|(instance, component)| ValidationComponent {
                instance: instance.clone(),
                source: component.component.clone(),
                has_driver: component.driver.is_some(),
                driver_checked: checked_instances.contains(instance),
            })
            .collect(),
    })
}

/// What `validate` is about to compile, in prose. The root brain is always
/// built; declared service crates and workspace component-driver crates are
/// built only when the robot declares any, so the brain-only case must read
/// naturally rather than announcing "0 declared service crates".
fn compile_notice(participants: &[SourceParticipant]) -> String {
    let count = |kinds: &[SourceParticipantKind]| {
        participants
            .iter()
            .filter(|participant| kinds.contains(&participant.kind))
            .count()
    };
    let mut parts = vec!["the root brain".to_string()];
    for (count, noun) in [
        (
            // A workspace crate that overrides an official service is a
            // declared service crate too: it is declared, it is built here, and
            // its schema is what the declared config is judged against.
            count(&[
                SourceParticipantKind::UserService,
                SourceParticipantKind::OfficialService,
            ]),
            "declared service crate",
        ),
        (
            count(&[SourceParticipantKind::ComponentDriver]),
            "component driver crate",
        ),
    ] {
        match count {
            0 => {}
            1 => parts.push(format!("1 {noun}")),
            many => parts.push(format!("{many} {noun}s")),
        }
    }
    let subject = match parts.as_slice() {
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => unreachable!("the root brain is always in the list"),
    };
    format!(
        "compiling {subject} to validate each against its embedded schema (first build may take \
         a while; cached afterward)"
    )
}

/// The registry-sourced artifacts this command can actually check, and the
/// report each one's staged binary carries.
///
/// Two kinds arrive here, for the same reason: a component driver, whose
/// authored `driver:` block only its own binary's contract can judge, and an
/// official service the document declares in order to configure it. Neither is
/// built from source by this command.
///
/// `validate` never stages and never installs. Such a binary is therefore
/// either already in this project's own release from an earlier `phoxal build` -
/// in which case its embedded contract is read straight off disk, exactly as
/// `build` and `run` read it - or it cannot be read here, and the honest answer
/// is to name what went unchecked rather than pass it silently or turn
/// `validate` into a build.
///
/// An official service the document does NOT declare never appears here at all:
/// it runs regardless, and there is nothing authored about it to be wrong.
struct StagedRegistryArtifacts {
    refs: Vec<PlatformArtifactRef>,
    reports: BTreeMap<String, RawParticipantReport>,
}

fn staged_registry_reports(
    project_root: &Path,
    robot: &Robot,
    resolved: &BundlePlan,
    project_framework: FrameworkVersion,
    reporter: &dyn crate::Reporter,
) -> StagedRegistryArtifacts {
    let bin_dir = crate::paths::runtime::runtime_release_root(project_root)
        .join(crate::deployment::BUNDLE_DIR)
        .join(phoxal::bundle::BIN_DIR);
    let mut runtimes = platform_runtimes_by_ref(resolved);
    runtimes.extend(component_driver_runtimes_by_ref(resolved));
    let mut staged = StagedRegistryArtifacts {
        refs: Vec::new(),
        reports: BTreeMap::new(),
    };
    let declared_officials = platform_artifact_refs_from_resolved(resolved)
        .into_iter()
        .filter(|official| robot.services.contains_key(&official.name));
    for artifact_ref in
        declared_officials.chain(component_driver_platform_refs_from_resolved(resolved))
    {
        let outcome = runtimes.get(&artifact_ref.binary_name).map(|runtime| {
            extract_participant_report_from_staged_runtime(&bin_dir, runtime, project_framework)
        });
        match outcome {
            Some(Ok(report)) => {
                staged
                    .reports
                    .insert(artifact_ref.binary_name.clone(), report);
                staged.refs.push(artifact_ref);
            }
            // A binary that is not here and a binary that is here but cannot
            // be read are two different answers, and only the second one has a
            // cause worth naming. Right after a framework bump the staged
            // release still holds the previous line's binaries, and calling
            // that "not staged" would be a plain untruth about a file sitting
            // on disk - and would hide the one sentence saying what to do.
            Some(Err(error)) if bin_dir.join(&artifact_ref.binary_name).exists() => {
                reporter.warn(unusable_artifact_notice(&artifact_ref, &error));
            }
            Some(Err(_)) | None => {
                reporter.warn(unstaged_artifact_notice(
                    &artifact_ref,
                    resolved.train.version(),
                ));
            }
        }
    }
    staged
}

/// What an unreadable binary leaves unchecked, as the subject of a sentence:
/// the driver blocks that binary was the only judge of, or the config the
/// document authored for that official service.
fn unchecked_subject(artifact_ref: &PlatformArtifactRef) -> String {
    match artifact_ref.kind {
        ArtifactKind::Service => format!("config of service `{}`", artifact_ref.name),
        ArtifactKind::ComponentDriver => {
            let instances = artifact_ref
                .instances
                .iter()
                .map(|instance| format!("`{instance}`"))
                .collect::<Vec<_>>()
                .join(", ");
            if artifact_ref.instances.len() == 1 {
                format!("driver block of {instances}")
            } else {
                format!("driver blocks of {instances}")
            }
        }
    }
}

/// The one thing an operator can act on when a registry binary is not here:
/// what went unchecked, which binary is missing, and the command that produces
/// it.
fn unstaged_artifact_notice(artifact_ref: &PlatformArtifactRef, train: &str) -> String {
    format!(
        "{} not validated: `{}` is not staged for train {train}; run `phoxal build`",
        unchecked_subject(artifact_ref),
        artifact_ref.binary_name,
    )
}

/// The same sentence for a binary that IS staged and still could not be read,
/// carrying the reason it could not.
///
/// The root cause is the one that names the actual disagreement - the train the
/// binary was built from, the document grammar it carries, the identity it
/// declares - while the layers above it only restate which file was being read,
/// which the notice already says. It is flattened to a single line because the
/// metadata reader's own guidance is a paragraph, and a warning is one line.
fn unusable_artifact_notice(artifact_ref: &PlatformArtifactRef, error: &anyhow::Error) -> String {
    let cause = error
        .root_cause()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // The cause is a sentence of its own; the clause after it supplies the
    // terminator, so a doubled one would only read as a stutter.
    let cause = cause.trim_end_matches('.');
    format!(
        "{} not validated: staged `{}` is unusable: {cause}; run `phoxal build`",
        unchecked_subject(artifact_ref),
        artifact_ref.binary_name,
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WorkspaceRuntimeReport {
    problems: Vec<String>,
    successes: Vec<String>,
}

/// Compare the declared `services:` map in robot.yaml against the
/// Cargo workspace's own discovered runtime crates - pure string comparison,
/// no compilation. `project` is the same locked resolution the public
/// [`crate::validate`] use case already computed; declaration invariants and the resolution are the
/// caller's responsibility (see the doc comment on
/// [`declared_config_source_participants`] for why: both checks read the same
/// `project.runtimes`, so resolving it once and sharing it here avoids a
/// second `cargo metadata --locked` invocation).
fn workspace_runtime_report(robot: &Robot, project: &LockedProject) -> WorkspaceRuntimeReport {
    let mut report = WorkspaceRuntimeReport::default();
    let services = project
        .runtimes
        .iter()
        .filter_map(|runtime| {
            runtime
                .crate_dir
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>();
    for service in &services {
        let note = if robot.services.contains_key(service) {
            "declared"
        } else {
            "undeclared - not part of the robot"
        };
        report.successes.push(format!(
            "Cargo workspace service '{service}' discovered ({note})"
        ));
    }
    let catalog = Catalog::official();
    for configured in robot.services.keys() {
        // An official identity under `services:` configures a service that runs
        // either way; it declares no workspace crate and owes none. A project
        // that DOES override an official's source has that crate under
        // `services/<id>` and it is discovered above like any other.
        if !services.contains(configured) && !catalog.is_official_service(configured) {
            report.problems.push(format!(
                "services.{configured} has no matching services/{configured} workspace crate"
            ));
        }
    }
    report
}

/// The mandatory root brain plus every workspace crate this project builds
/// that carries an authored configuration surface.
///
/// The brain carries no config at all (`#[phoxal::brain]` fixes `Config = ()`),
/// but it is built and inspected here anyway so `phoxal validate` proves the
/// root package really is a brain - the right id, kind, and unit config schema -
/// before anything else depends on it.
///
/// The config-bearing half is two things. A `services/<id>` crate whose `<id>`
/// is a declared key in `robot.services` - either a service this project owns
/// outright, or a workspace crate that path-overrides an official one, which
/// is the very binary that will read the declared config and therefore the one
/// whose schema it must satisfy. And every `components/<id>` crate this project
/// owns the source of, whose driven instances author a `driver:` block the
/// crate's own binary has to accept; a registry-sourced driver is not built
/// here at all (see [`staged_registry_reports`]). This scoping is the point:
/// `validate` builds only the crates whose contract it actually needs to read,
/// not the whole official graph `build`/`run` resolve.
fn declared_source_participants(
    robot: &Robot,
    project: &LockedProject,
    resolved: &BundlePlan,
) -> Vec<SourceParticipant> {
    let catalog = Catalog::official();
    let mut participants = vec![SourceParticipant::brain(
        project.brain.crate_dir.clone(),
        project.brain.bin_target.clone(),
    )];
    participants.extend(project.runtimes.iter().filter_map(|runtime| {
        let name = runtime.crate_dir.file_name()?.to_str()?.to_string();
        if !robot.services.contains_key(&name) {
            return None;
        }
        // The two flavours are checked identically but must not be labelled
        // identically: calling a path-overridden official a "user service" in a
        // build or identity diagnostic would name the wrong thing.
        Some(if catalog.is_official_service(&name) {
            SourceParticipant::official_service(name.clone(), name, runtime.crate_dir.clone())
        } else {
            SourceParticipant::user_service(name, runtime.crate_dir.clone())
        })
    }));
    participants.extend(resolved.components.iter().filter_map(|component| {
        match component.driver.as_ref()? {
            ResolvedComponentDriver::Local { crate_dir } => {
                Some(SourceParticipant::component_driver_with_artifact_id(
                    component.instance.clone(),
                    component.source_name.clone(),
                    crate_dir.clone(),
                ))
            }
            ResolvedComponentDriver::Registry(_) => None,
        }
    }));
    participants
}

/// Check every declared source participant plus every registry artifact whose
/// binary is already staged, through the shared check engine
/// ([`crate::validation::run_check_with_context`]) `build`/`run`/`simulate`
/// already use - never a second, divergent validator. `fetch` and `build` are
/// injected so pure tests can supply reports; production reads each staged
/// binary off disk and looks up each built binary in the one selected-source
/// artifact batch prepared by [`crate::validate`].
fn check_declared_sources(
    robot: &Robot,
    platform_refs: &[PlatformArtifactRef],
    source_participants: &[SourceParticipant],
    fetch: impl FnMut(&str) -> Result<RawParticipantReport>,
    build: impl FnMut(&SourceParticipant) -> Result<RawParticipantReport>,
) -> Result<CheckOutcome> {
    run_check_with_context(
        platform_refs,
        source_participants,
        CheckGraphContext { robot: Some(robot) },
        fetch,
        build,
    )
}

fn join_errors(errors: Vec<phoxal::authoring::source::robot::v0::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::train::{LockedTrain, WorkspaceRuntime};
    use crate::validation::{RawArtifact, RawParticipantReport};
    use anyhow::bail;
    use std::path::PathBuf;

    fn locked_project(runtimes: Vec<WorkspaceRuntime>) -> LockedProject {
        LockedProject {
            train: LockedTrain::from_locked_version("0.42.0")
                .expect("the fixture locks a canonical framework version"),
            brain: crate::source::train::RootBrainPackage {
                package: "testbot-robot".to_string(),
                crate_dir: PathBuf::from("/fake/project"),
                bin_target: "testbot-robot".to_string(),
            },
            runtimes,
            local_components: Vec::new(),
        }
    }

    /// A resolved plan carrying exactly `components`, so the source-participant
    /// selection can be driven without a real resolution.
    fn resolved_with_components(
        components: Vec<crate::source::resolver::ResolvedComponent>,
    ) -> Result<BundlePlan> {
        let robot = minimal_robot();
        Ok(BundlePlan {
            compiled: crate::stage::compile_test_bundle(&robot)?,
            source_manifest: robot,
            train: crate::stage::test_project_train(),
            target: crate::resolve::project::host_target_triple(),
            brain: crate::source::resolver::ResolvedBrain {
                crate_dir: PathBuf::from("/fake/project"),
                package: "testbot-robot".to_string(),
                bin_target: "testbot-robot".to_string(),
            },
            platform_runtimes: Vec::new(),
            user_runtimes: Vec::new(),
            undeclared_runtimes: Vec::new(),
            components,
            path_overrides: Vec::new(),
        })
    }

    fn workspace_runtime(dir: &str) -> WorkspaceRuntime {
        let crate_dir = PathBuf::from(format!("/fake/project/{dir}"));
        let name = crate_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        WorkspaceRuntime {
            package: name.clone(),
            crate_dir,
            binary_names: vec![name],
        }
    }

    const MINIMAL_ROBOT: &str = r#"schema: phoxal/robot/v0
robot:
  id: bot
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: [drive.motor]
    encoders: []
  components:
    drive:
      component: wheel
      mount_link: base
services:
  avoid: {}
"#;

    fn minimal_robot() -> Robot {
        crate::source::resolver::parse_robot_from_string(MINIMAL_ROBOT)
            .expect("minimal fixture robot.yaml parses")
    }

    fn robot_with_service_config(config: serde_json::Value) -> Robot {
        let mut robot = minimal_robot();
        robot
            .services
            .get_mut("avoid")
            .expect("avoid service")
            .config = Some(config);
        robot
    }

    fn raw_service(id: &str, schema: serde_json::Value) -> RawParticipantReport {
        RawParticipantReport {
            artifact: RawArtifact {
                kind: "service".to_string(),
                id: id.to_string(),
            },
            config_schema: Some(schema),
            connection: None,
        }
    }

    /// The brain-only project is the common case for a fresh robot, so its
    /// notice must not mention zero service crates - and what it announces is
    /// exactly what the build batch contains, drivers included.
    #[test]
    fn the_compile_notice_names_exactly_what_is_compiled() {
        let brain = SourceParticipant::brain(PathBuf::from("/fake/project"), "testbot-robot");
        let service = |name: &str| {
            SourceParticipant::user_service(name, PathBuf::from(format!("/fake/project/{name}")))
        };
        let driver = |instance: &str| {
            SourceParticipant::component_driver_with_artifact_id(
                instance,
                "ddsm115",
                PathBuf::from("/fake/project/components/ddsm115"),
            )
        };

        assert!(
            compile_notice(std::slice::from_ref(&brain))
                .starts_with("compiling the root brain to validate")
        );
        assert!(
            compile_notice(&[brain.clone(), service("avoid")])
                .contains("the root brain and 1 declared service crate to")
        );
        assert!(
            compile_notice(&[brain.clone(), service("avoid"), service("mission")])
                .contains("the root brain and 2 declared service crates to")
        );
        assert!(
            compile_notice(&[brain.clone(), driver("left_drive")])
                .contains("the root brain and 1 component driver crate to")
        );
        assert!(
            compile_notice(&[
                brain,
                service("avoid"),
                driver("left_drive"),
                driver("right_drive")
            ])
            .contains("the root brain, 1 declared service crate and 2 component driver crates to")
        );
    }

    #[test]
    fn workspace_runtime_report_flags_missing_and_notes_undeclared_crates() {
        let robot = minimal_robot();
        let project = locked_project(vec![
            workspace_runtime("services/avoid"),
            workspace_runtime("services/extra"),
        ]);
        let report = workspace_runtime_report(&robot, &project);
        assert!(
            report
                .successes
                .iter()
                .any(|success| success.contains("avoid") && success.contains("(declared)")),
            "{report:?}"
        );
        assert!(
            report
                .successes
                .iter()
                .any(|success| success.contains("extra") && success.contains("undeclared")),
            "{report:?}"
        );
    }

    /// The build batch is the brain, the declared services, and every driver
    /// this project owns the source of - keyed by its component instance,
    /// because that is what its `driver:` block is attached to. A registry
    /// driver is not source and is never built here.
    #[test]
    fn declared_source_participants_covers_the_brain_declared_services_and_local_drivers()
    -> Result<()> {
        let robot = minimal_robot();
        let project = locked_project(vec![
            workspace_runtime("services/avoid"),
            // Present on disk but not declared in robot.services - a drift
            // diagnostic elsewhere, never a config-check participant here.
            workspace_runtime("services/undeclared"),
        ]);
        let resolved = resolved_with_components(vec![
            crate::source::resolver::ResolvedComponent {
                instance: "left_drive".to_string(),
                source_name: "ddsm115".to_string(),
                assets_root: PathBuf::from("components/ddsm115"),
                driver: Some(ResolvedComponentDriver::Local {
                    crate_dir: PathBuf::from("/fake/project/components/ddsm115"),
                }),
            },
            crate::source::resolver::ResolvedComponent {
                instance: "right_drive".to_string(),
                source_name: "vl53l1x".to_string(),
                assets_root: PathBuf::from("registry/vl53l1x"),
                driver: Some(ResolvedComponentDriver::Registry(
                    crate::source::resolver::ResolvedPlatformRuntime {
                        name: "vl53l1x".to_string(),
                        package: "phoxal/component-vl53l1x".to_string(),
                        kind: ArtifactKind::ComponentDriver,
                        path_override: None,
                        train: "0.42.0".to_string(),
                        target: None,
                    },
                )),
            },
            crate::source::resolver::ResolvedComponent {
                instance: "caster".to_string(),
                source_name: "passive_caster".to_string(),
                assets_root: PathBuf::from("components/passive_caster"),
                driver: None,
            },
        ])?;

        let participants = declared_source_participants(&robot, &project, &resolved);

        assert_eq!(participants.len(), 3, "{participants:?}");
        let brain = &participants[0];
        assert_eq!(brain.name, "brain");
        assert_eq!(brain.kind, SourceParticipantKind::Brain);
        assert_eq!(brain.crate_dir, PathBuf::from("/fake/project"));
        assert_eq!(brain.bin_target.as_deref(), Some("testbot-robot"));
        assert!(
            participants
                .iter()
                .any(|participant| participant.name == "avoid"
                    && participant.kind == SourceParticipantKind::UserService)
        );
        let driver = participants
            .iter()
            .find(|participant| participant.kind == SourceParticipantKind::ComponentDriver)
            .expect("the workspace driver enters the build batch");
        assert_eq!(driver.name, "left_drive");
        assert_eq!(driver.expected_artifact_id, "ddsm115");
        assert_eq!(
            driver.crate_dir,
            PathBuf::from("/fake/project/components/ddsm115")
        );
        Ok(())
    }

    /// The unchecked-artifact warning has to be actionable on its own: what
    /// went unchecked, which binary is missing, and what produces it. A driver
    /// names the instances whose blocks it was the only judge of; a declared
    /// official names the service whose config went unread.
    #[test]
    fn an_unstaged_registry_artifact_names_its_subject_binary_and_the_fix() {
        let one = driver_ref(vec!["left_drive".to_string()]);
        assert_eq!(
            unstaged_artifact_notice(&one, "0.42.0"),
            "driver block of `left_drive` not validated: `ddsm115` is not staged for train \
             0.42.0; run `phoxal build`"
        );

        let two = driver_ref(vec!["left_drive".to_string(), "right_drive".to_string()]);
        assert_eq!(
            unstaged_artifact_notice(&two, "0.42.0"),
            "driver blocks of `left_drive`, `right_drive` not validated: `ddsm115` is not staged \
             for train 0.42.0; run `phoxal build`"
        );

        assert_eq!(
            unstaged_artifact_notice(&official_ref("safety"), "0.42.0"),
            "config of service `safety` not validated: `safety` is not staged for train 0.42.0; \
             run `phoxal build`"
        );
    }

    fn driver_ref(instances: Vec<String>) -> PlatformArtifactRef {
        PlatformArtifactRef {
            name: "ddsm115".to_string(),
            kind: ArtifactKind::ComponentDriver,
            binary_name: "ddsm115".to_string(),
            instances,
        }
    }

    fn official_ref(name: &str) -> PlatformArtifactRef {
        PlatformArtifactRef {
            name: name.to_string(),
            kind: ArtifactKind::Service,
            binary_name: phoxal_cli_catalog::bundle_binary_name(name),
            instances: Vec::new(),
        }
    }

    #[derive(Default)]
    struct RecordingReporter(std::sync::Mutex<Vec<crate::PreparationEvent>>);

    impl crate::Reporter for RecordingReporter {
        fn report(&self, event: crate::PreparationEvent) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
    }

    impl RecordingReporter {
        fn warnings(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter_map(|event| match event {
                    crate::PreparationEvent::Warn(message) => Some(message.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    /// A plan whose one component instance is driven by a registry-sourced
    /// `ddsm115`, which is what `staged_registry_reports` reasons about.
    fn resolved_with_registry_driver() -> Result<BundlePlan> {
        resolved_with_components(vec![crate::source::resolver::ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets_root: PathBuf::from("registry/ddsm115"),
            driver: Some(ResolvedComponentDriver::Registry(
                crate::source::resolver::ResolvedPlatformRuntime {
                    name: "ddsm115".to_string(),
                    package: "phoxal/component-ddsm115".to_string(),
                    kind: ArtifactKind::ComponentDriver,
                    path_override: None,
                    train: crate::check::participant_metadata::FIXTURE_FRAMEWORK.to_string(),
                    target: None,
                },
            )),
        }])
    }

    /// A plan carrying one registry-resolved official service, the shape a real
    /// resolution produces for every catalog entry.
    fn resolved_with_official_service(name: &str) -> Result<BundlePlan> {
        let mut resolved = resolved_with_components(Vec::new())?;
        resolved
            .platform_runtimes
            .push(crate::source::resolver::ResolvedPlatformRuntime {
                name: name.to_string(),
                package: format!("phoxal/service-{name}"),
                kind: ArtifactKind::Service,
                path_override: None,
                train: crate::check::participant_metadata::FIXTURE_FRAMEWORK.to_string(),
                target: None,
            });
        Ok(resolved)
    }

    /// The minimal fixture document with `services.<id>` declared, so an
    /// official identity under `services:` can be exercised.
    fn robot_declaring(id: &str) -> Robot {
        crate::source::resolver::parse_robot_from_string(
            &MINIMAL_ROBOT.replace("  avoid: {}", &format!("  {id}: {{}}")),
        )
        .expect("the fixture robot.yaml parses")
    }

    /// Write `bytes` where this project's own release stages `bin/<name>`.
    fn stage_binary(project_root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
        let bin = crate::paths::runtime::runtime_release_root(project_root)
            .join(crate::deployment::BUNDLE_DIR)
            .join(phoxal::bundle::BIN_DIR);
        std::fs::create_dir_all(&bin)?;
        std::fs::write(bin.join(name), bytes)?;
        Ok(())
    }

    /// The metadata object a built `<id>` binary of `kind` embeds.
    fn staged_metadata(id: &str, kind: &str) -> Vec<u8> {
        crate::check::participant_metadata::synthesize_host_participant_object(
            &crate::stage::test_metadata_payload(
                id,
                kind,
                (kind == "driver").then_some(phoxal::model::connection::ConnectionKind::Serial),
                serde_json::json!({"type": "null"}),
            ),
        )
    }

    /// Nothing staged: the binary is genuinely absent, so the notice says so
    /// and the ref is left out of the check.
    #[test]
    fn a_registry_driver_with_no_staged_binary_is_reported_unstaged() -> Result<()> {
        let project = tempfile::tempdir()?;
        let resolved = resolved_with_registry_driver()?;
        let reporter = RecordingReporter::default();

        let staged = staged_registry_reports(
            project.path(),
            &minimal_robot(),
            &resolved,
            crate::check::participant_metadata::FIXTURE_FRAMEWORK,
            &reporter,
        );

        assert!(staged.refs.is_empty(), "{:?}", staged.refs);
        assert!(staged.reports.is_empty());
        assert_eq!(
            reporter.warnings(),
            vec![format!(
                "driver block of `left_drive` not validated: `ddsm115` is not staged for train \
                 {}; run `phoxal build`",
                crate::check::participant_metadata::FIXTURE_FRAMEWORK
            )]
        );
        Ok(())
    }

    /// The ordinary case right after a framework bump: the release still holds
    /// the previous line's driver. The file is right there, so calling it "not
    /// staged" would be untrue and would hide the reason it cannot be read.
    #[test]
    fn a_staged_driver_from_another_train_is_reported_unusable_with_its_cause() -> Result<()> {
        let project = tempfile::tempdir()?;
        stage_binary(
            project.path(),
            "ddsm115",
            &staged_metadata("ddsm115", "driver"),
        )?;
        let resolved = resolved_with_registry_driver()?;
        let reporter = RecordingReporter::default();

        // The project has moved a compatibility line ahead of what its own
        // release last staged.
        let staged = staged_registry_reports(
            project.path(),
            &minimal_robot(),
            &resolved,
            FrameworkVersion::new(0, 43, 0),
            &reporter,
        );

        assert!(staged.refs.is_empty(), "{:?}", staged.refs);
        assert!(staged.reports.is_empty());
        let [warning] = reporter.warnings().try_into().expect("exactly one warning");
        assert!(
            warning.starts_with(
                "driver block of `left_drive` not validated: staged `ddsm115` is unusable: "
            ),
            "{warning}"
        );
        assert!(
            warning.contains(&format!(
                "was built from phoxal framework {}",
                crate::check::participant_metadata::FIXTURE_FRAMEWORK
            )),
            "{warning}"
        );
        assert!(warning.contains("0.43.0"), "{warning}");
        assert!(
            warning.ends_with("or rebuild the binary; run `phoxal build`"),
            "{warning}"
        );
        assert_eq!(warning.lines().count(), 1, "{warning}");
        Ok(())
    }

    /// A staged binary the project's own line accepts is read here, and its
    /// ref enters the check with the report that binary carries.
    #[test]
    fn a_staged_driver_on_the_projects_line_is_read_and_checked() -> Result<()> {
        let project = tempfile::tempdir()?;
        stage_binary(
            project.path(),
            "ddsm115",
            &staged_metadata("ddsm115", "driver"),
        )?;
        let resolved = resolved_with_registry_driver()?;
        let reporter = RecordingReporter::default();

        let staged = staged_registry_reports(
            project.path(),
            &minimal_robot(),
            &resolved,
            crate::check::participant_metadata::FIXTURE_FRAMEWORK,
            &reporter,
        );

        assert!(reporter.warnings().is_empty(), "{:?}", reporter.warnings());
        assert_eq!(staged.refs.len(), 1);
        assert_eq!(staged.refs[0].instances, ["left_drive"]);
        let report = staged.reports.get("ddsm115").expect("the staged report");
        assert_eq!(report.artifact.id, "ddsm115");
        assert_eq!(
            report.connection,
            Some(phoxal::model::connection::ConnectionKind::Serial)
        );
        Ok(())
    }

    /// An official service the document never mentions has nothing authored to
    /// check, so `validate` neither reads its binary nor warns about it - it
    /// runs regardless, and a warning about a file nobody asked a question of
    /// would be noise on every project.
    #[test]
    fn an_undeclared_official_service_is_never_read_and_never_warned_about() -> Result<()> {
        let project = tempfile::tempdir()?;
        let resolved = resolved_with_official_service("safety")?;
        let reporter = RecordingReporter::default();

        let staged = staged_registry_reports(
            project.path(),
            &minimal_robot(),
            &resolved,
            crate::check::participant_metadata::FIXTURE_FRAMEWORK,
            &reporter,
        );

        assert!(staged.refs.is_empty(), "{:?}", staged.refs);
        assert!(reporter.warnings().is_empty(), "{:?}", reporter.warnings());
        Ok(())
    }

    /// A declared official whose binary this project has not staged: the
    /// config an operator wrote goes unread, and the notice has to say so
    /// rather than let `validate` report the project green.
    #[test]
    fn a_declared_official_service_with_no_staged_binary_is_reported_unstaged() -> Result<()> {
        let project = tempfile::tempdir()?;
        let resolved = resolved_with_official_service("safety")?;
        let reporter = RecordingReporter::default();

        let staged = staged_registry_reports(
            project.path(),
            &robot_declaring("safety"),
            &resolved,
            crate::check::participant_metadata::FIXTURE_FRAMEWORK,
            &reporter,
        );

        assert!(staged.refs.is_empty(), "{:?}", staged.refs);
        assert_eq!(
            reporter.warnings(),
            vec![format!(
                "config of service `safety` not validated: `safety` is not staged for train {}; \
                 run `phoxal build`",
                crate::check::participant_metadata::FIXTURE_FRAMEWORK
            )]
        );
        Ok(())
    }

    /// The same declared official, with its binary staged on this project's own
    /// line: its embedded contract is read straight off disk and its ref enters
    /// the check, exactly as a staged driver's does.
    #[test]
    fn a_declared_official_service_staged_on_the_projects_line_is_read_and_checked() -> Result<()> {
        let project = tempfile::tempdir()?;
        stage_binary(
            project.path(),
            "safety",
            &staged_metadata("safety", "service"),
        )?;
        let resolved = resolved_with_official_service("safety")?;
        let reporter = RecordingReporter::default();

        let staged = staged_registry_reports(
            project.path(),
            &robot_declaring("safety"),
            &resolved,
            crate::check::participant_metadata::FIXTURE_FRAMEWORK,
            &reporter,
        );

        assert!(reporter.warnings().is_empty(), "{:?}", reporter.warnings());
        assert_eq!(staged.refs.len(), 1);
        assert_eq!(staged.refs[0].name, "safety");
        assert!(
            staged.refs[0].instances.is_empty(),
            "a service is not keyed per instance: {:?}",
            staged.refs[0]
        );
        let report = staged.reports.get("safety").expect("the staged report");
        assert_eq!(report.artifact.id, "safety");
        assert_eq!(report.artifact.kind, "service");
        Ok(())
    }

    /// A declared official whose binary IS staged but on another line: the file
    /// is right there, so "not staged" would be untrue and would hide the
    /// reason its config could not be checked.
    #[test]
    fn a_declared_official_service_from_another_train_is_reported_unusable() -> Result<()> {
        let project = tempfile::tempdir()?;
        stage_binary(
            project.path(),
            "safety",
            &staged_metadata("safety", "service"),
        )?;
        let resolved = resolved_with_official_service("safety")?;
        let reporter = RecordingReporter::default();

        let staged = staged_registry_reports(
            project.path(),
            &robot_declaring("safety"),
            &resolved,
            FrameworkVersion::new(0, 43, 0),
            &reporter,
        );

        assert!(staged.refs.is_empty(), "{:?}", staged.refs);
        let [warning] = reporter.warnings().try_into().expect("exactly one warning");
        assert!(
            warning.starts_with(
                "config of service `safety` not validated: staged `safety` is unusable: "
            ),
            "{warning}"
        );
        assert!(warning.contains("0.43.0"), "{warning}");
        assert_eq!(warning.lines().count(), 1, "{warning}");
        Ok(())
    }

    /// The core end-to-end shape the maintainer asked for: a config that
    /// violates its participant's embedded schema must fail the check, and a
    /// valid one must pass, wired through the exact function [`crate::validate`]
    /// calls - `check_declared_sources` - with a fake (no `cargo`, no network)
    /// build closure standing in for the real compile.
    #[test]
    fn a_config_violating_its_schema_fails_and_a_valid_one_passes() -> Result<()> {
        let participants = vec![SourceParticipant::user_service(
            "avoid",
            PathBuf::from("/fake/project/services/avoid"),
        )];
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "gain": { "type": "number" } },
            "required": ["gain"],
        });

        let robot = robot_with_service_config(serde_json::json!({ "gain": "fast" }));
        let outcome = check_declared_sources(
            &robot,
            &[],
            &participants,
            |image_ref| bail!("no staged driver is read here (got {image_ref})"),
            |_| Ok(raw_service("avoid", schema.clone())),
        )?;
        assert!(
            !outcome.is_ok(),
            "a mistyped gain must be rejected: {outcome:?}"
        );
        assert!(
            outcome
                .report
                .problems
                .iter()
                .any(|problem| format!("{problem:?}").contains("avoid")),
            "{outcome:?}"
        );

        let robot = robot_with_service_config(serde_json::json!({ "gain": 1.5 }));
        let outcome = check_declared_sources(
            &robot,
            &[],
            &participants,
            |image_ref| bail!("no staged driver is read here (got {image_ref})"),
            |_| Ok(raw_service("avoid", schema.clone())),
        )?;
        assert!(outcome.is_ok(), "a valid gain must pass: {outcome:?}");
        Ok(())
    }

    #[test]
    fn no_declared_source_participants_means_no_build_is_requested() -> Result<()> {
        let robot = minimal_robot();
        let outcome = check_declared_sources(
            &robot,
            &[],
            &[],
            |image_ref| bail!("no staged driver is read here (got {image_ref})"),
            |participant: &SourceParticipant| {
                panic!("no participant should be built, got {}", participant.name)
            },
        )?;
        assert!(outcome.is_ok(), "{outcome:?}");
        Ok(())
    }
}
