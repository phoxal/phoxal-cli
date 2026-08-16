use std::collections::BTreeSet;

use crate::check::source::SourceParticipant;
use crate::source::train::LockedProject;
use anyhow::{Context, Result, anyhow, bail};
use phoxal_cli_catalog::{ArtifactKind, Catalog};
use phoxal_manifest::source::robot::v0::Manifest as Robot;

use super::{ValidateRequest, ValidationComponent, ValidationReport, ValidationSource};
use crate::validation::{
    CheckGraphContext, CheckOutcome, RawParticipantReport, build_participant_report_from_binary,
    ensure_check_outcome_ok, run_check_with_context,
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
                robot_path: release.bundle.join(phoxal_bundle::MANIFEST_FILE),
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

    // Declaration-only invariants first, matching resolution's
    // ordering: a dual-name or official-identity declaration must fail
    // before any Cargo workspace reasoning, source-check or otherwise.
    crate::resolve::project::validate_runtime_declarations(&robot)
        .map_err(|error| anyhow!("Cargo workspace runtime discovery failed:\n{error:#}"))?;

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

    // The mandatory root brain plus config-schema validation ensure every declared user
    // service's `services.<id>.config` must satisfy the JSON Schema its own
    // `#[phoxal::service(config = ...)]` type embeds. There is no schema
    // until that type compiles, so
    // this is the one part of `validate` that is not free - it builds
    // the root brain and ONLY the declared participant crates (never the
    // official set, never a staged bundle), through the same check engine
    // `build`/`run`/`simulate` already use.
    let config_participants = declared_config_source_participants(&robot, &project);
    request.reporter.info(compile_notice(
        config_participants
            .iter()
            .filter(|participant| {
                participant.kind == crate::check::source::SourceParticipantKind::UserService
            })
            .count(),
    ));
    let artifacts = crate::build::cargo::build_selected_source_artifacts(
        &config_participants,
        None,
        crate::build::profile::Profile::Debug,
        None,
        request.offline,
        request.reporter.as_ref(),
    )?;
    let outcome = check_declared_configs(&robot, &config_participants, |participant| {
        build_participant_report_from_binary(
            participant,
            artifacts.binary(participant)?,
            project_framework,
            request.reporter.as_ref(),
        )
    })?;
    ensure_check_outcome_ok(&outcome)?;
    Ok(ValidationReport {
        robot_path,
        robot: robot.robot.id.clone(),
        train,
        platform_services: Catalog::official()
            .native()
            .filter(|official| official.kind == ArtifactKind::Service)
            .map(|official| official.package.to_string())
            .collect(),
        services: robot.services.keys().cloned().collect(),
        components: robot
            .robot
            .components
            .iter()
            .map(|(instance, component)| ValidationComponent {
                instance: instance.clone(),
                source: component.component.clone(),
                has_driver: component.driver.is_some(),
            })
            .collect(),
    })
}

/// What `validate` is about to compile, in prose. The root brain is always
/// built; declared service crates are built only when the
/// robot declares any, so the brain-only case must read naturally rather than
/// announcing "0 declared service crates".
fn compile_notice(declared_services: usize) -> String {
    let subject = match declared_services {
        0 => "the root brain".to_string(),
        1 => "the root brain and 1 declared service crate".to_string(),
        count => format!("the root brain and {count} declared service crates"),
    };
    format!(
        "compiling {subject} to validate each against its embedded schema (first build may take \
         a while; cached afterward)"
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
    for configured in robot.services.keys() {
        if !services.contains(configured) {
            report.problems.push(format!(
                "services.{configured} has no matching services/{configured} workspace crate"
            ));
        }
    }
    report
}

/// The mandatory root brain plus every workspace runtime crate that can
/// legally carry a robot.yaml `config:` value.
///
/// The brain carries no config at all (`#[phoxal::brain]` fixes `Config = ()`),
/// but it is built and inspected here anyway so `phoxal validate` proves the
/// root package really is a brain - the right id, kind, and unit config schema -
/// before anything else depends on it.
///
/// The config-bearing half is a `services/<id>` crate whose `<id>` is a declared key in
/// `robot.services`. An official identity can never be a declared key
/// (`validate_runtime_declarations` rejects that at parse time), so this
/// filter alone is enough to exclude a workspace crate that path-overrides an
/// official service - no separate "is this an official package name" lookup is
/// needed here the way full graph resolution needs one. This scoping is the
/// point (
/// `validate` builds only the crates whose schema
/// it actually needs to read, not the whole component/driver/official graph
/// `build`/`run` resolve.
fn declared_config_source_participants(
    robot: &Robot,
    project: &LockedProject,
) -> Vec<SourceParticipant> {
    let mut participants = vec![SourceParticipant::brain(
        project.brain.crate_dir.clone(),
        project.brain.bin_target.clone(),
    )];
    participants.extend(project.runtimes.iter().filter_map(|runtime| {
        let name = runtime.crate_dir.file_name()?.to_str()?.to_string();
        robot
            .services
            .contains_key(&name)
            .then(|| SourceParticipant::user_service(name, runtime.crate_dir.clone()))
    }));
    participants
}

/// Build every declared config-bearing participant and validate its
/// robot.yaml config against its own emitted schema, through the shared check
/// engine ([`crate::validation::run_check_with_context`]) `build`/`run`/`simulate`
/// already use - never a second, divergent validator. `build` is injected so
/// pure tests can supply reports; production looks up each report in the one
/// selected-source artifact batch prepared by [`crate::validate`].
fn check_declared_configs(
    robot: &Robot,
    source_participants: &[SourceParticipant],
    build: impl FnMut(&SourceParticipant) -> Result<RawParticipantReport>,
) -> Result<CheckOutcome> {
    run_check_with_context(
        &[],
        source_participants,
        CheckGraphContext { robot: Some(robot) },
        |image_ref| {
            bail!(
                "validate does not fetch official artifact reports (unexpected request for \
                 {image_ref})"
            )
        },
        build,
    )
}

fn join_errors(errors: Vec<phoxal_manifest::source::robot::v0::ValidationError>) -> String {
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
        }
    }

    /// The brain-only project is the common case for a fresh robot, so its
    /// notice must not mention zero service crates.
    #[test]
    fn the_compile_notice_reads_naturally_for_every_declared_service_count() {
        assert!(compile_notice(0).starts_with("compiling the root brain to validate"));
        assert!(compile_notice(1).contains("and 1 declared service crate to"));
        assert!(compile_notice(4).contains("and 4 declared service crates to"));
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

    #[test]
    fn declared_config_source_participants_covers_the_brain_and_only_declared_services() {
        let robot = minimal_robot();
        let project = locked_project(vec![
            workspace_runtime("services/avoid"),
            // Present on disk but not declared in robot.services - a drift
            // diagnostic elsewhere, never a config-check participant here.
            workspace_runtime("services/undeclared"),
        ]);

        let participants = declared_config_source_participants(&robot, &project);

        // The mandatory root brain plus the one declared service - never the
        // undeclared drift crate (, ).
        assert_eq!(participants.len(), 2, "{participants:?}");
        let brain = &participants[0];
        assert_eq!(brain.name, "brain");
        assert_eq!(
            brain.kind,
            crate::check::source::SourceParticipantKind::Brain
        );
        assert_eq!(brain.crate_dir, PathBuf::from("/fake/project"));
        assert_eq!(brain.bin_target.as_deref(), Some("testbot-robot"));
        assert!(
            participants
                .iter()
                .any(|participant| participant.name == "avoid"
                    && participant.kind
                        == crate::check::source::SourceParticipantKind::UserService)
        );
    }

    /// The core end-to-end shape the maintainer asked for: a config that
    /// violates its participant's embedded schema must fail the check, and a
    /// valid one must pass, wired through the exact function [`crate::validate`]
    /// calls - `check_declared_configs` - with a fake (no `cargo`, no
    /// network) build closure standing in for the real compile, matching how
    /// `crates/project/src/validation/tests.rs` fakes the same seam.
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
        let outcome = check_declared_configs(&robot, &participants, |_| {
            Ok(raw_service("avoid", schema.clone()))
        })?;
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
        let outcome = check_declared_configs(&robot, &participants, |_| {
            Ok(raw_service("avoid", schema.clone()))
        })?;
        assert!(outcome.is_ok(), "a valid gain must pass: {outcome:?}");
        Ok(())
    }

    #[test]
    fn no_declared_config_participants_means_no_build_is_requested() -> Result<()> {
        let robot = minimal_robot();
        let outcome = check_declared_configs(&robot, &[], |participant: &SourceParticipant| {
            panic!("no participant should be built, got {}", participant.name)
        })?;
        assert!(outcome.is_ok(), "{outcome:?}");
        Ok(())
    }
}
