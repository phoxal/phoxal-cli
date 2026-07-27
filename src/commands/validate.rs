use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use phoxal::model::robot::RobotV0 as Robot;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::project::catalog::{self, ArtifactKind};
use phoxal_cli_core::project::train::{LockedProject, WorkspaceRuntimeKind};

use crate::AppContext;
use crate::check::{
    CheckGraphContext, CheckOutcome, RawParticipantReport, build_participant_report_by_building,
    build_participant_report_from_source_with_diagnostics, ensure_check_outcome_ok,
    run_check_with_context,
};

#[derive(Debug, Args)]
pub struct Validate {
    #[arg(long, help = "Print the derived service/component graph.")]
    pub report: bool,
    #[arg(
        long,
        value_enum,
        default_value_t = ReportFormat::Text,
        help = "Format for the --report output."
    )]
    pub report_format: ReportFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ReportFormat {
    Text,
    Json,
}

impl Validate {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let robot_path =
            phoxal_cli_core::project::resolver::discover_robot_yaml(app.project.root())?;
        let project_root = robot_path
            .parent()
            .context("robot.yaml did not have a parent directory")?;
        let robot = phoxal_cli_core::project::resolver::load_robot(&robot_path)?;
        robot
            .validate()
            .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;

        // Declaration-only invariants first (#950), matching resolution's
        // ordering: a dual-name or official-identity declaration must fail
        // before any Cargo workspace reasoning, source-check or otherwise.
        phoxal_cli_core::project::layout::validate_runtime_declarations(&robot)
            .map_err(|error| anyhow!("Cargo workspace runtime discovery failed:\n{error:#}"))?;

        // One locked-workspace resolution feeds both the structural workspace
        // check below AND the config-schema check that follows it - no suite
        // fetch, no network beyond what a normal `cargo metadata --locked`
        // needs (organization#951 WS4).
        let project = phoxal_cli_core::project::train::resolve_locked_project(
            project_root,
            app.offline,
        )
        .map_err(|error| {
            anyhow!(
                "Cargo workspace runtime discovery failed:\nlocked Cargo workspace is invalid: {error:#}"
            )
        })?;
        let train = project.train.version.clone();
        let platform_names = catalog::NATIVE
            .iter()
            .filter(|official| official.kind == ArtifactKind::Service)
            .count();

        let workspace = workspace_runtime_report(&robot, &project);
        for success in &workspace.successes {
            app.ui.success(success.clone());
        }
        anyhow::ensure!(
            workspace.problems.is_empty(),
            "Cargo workspace runtime discovery failed:\n{}",
            workspace.problems.join("\n")
        );

        // Config-schema validation (#951 WS4 follow-up): every declared user
        // service/tool's `<family>.<id>.config` must satisfy the JSON Schema
        // its own `#[phoxal::service(config = ...)]`/`#[phoxal::tool(config =
        // ...)]` type embeds. There is no schema until that type compiles, so
        // this is the one part of `validate` that is not free - it builds
        // ONLY the declared participant crates (never the official set, never
        // a staged bundle), through the same check engine `build`/`run`/
        // `simulate` already use.
        let config_participants = declared_config_source_participants(&robot, &project);
        if !config_participants.is_empty() {
            app.ui.info(format!(
                "compiling {} declared service/tool crate{} to validate config against its \
                 embedded schema (first build may take a while; cached afterward)",
                config_participants.len(),
                if config_participants.len() == 1 {
                    ""
                } else {
                    "s"
                },
            ));
        }
        let offline = app.offline;
        let outcome = check_declared_configs(&robot, &config_participants, |participant| {
            build_participant_report_from_source_with_diagnostics(
                participant,
                |participant| build_participant_report_by_building(participant, offline),
                Some(&app.ui),
            )
        })?;
        ensure_check_outcome_ok(&outcome)?;

        app.ui.success(format!(
            "validated {} with {} official services",
            robot_path.display(),
            platform_names
        ));
        if self.report {
            match self.report_format {
                ReportFormat::Text => print_text_report(&robot, &train),
                ReportFormat::Json => print_json_report(&robot, &train)?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WorkspaceRuntimeReport {
    problems: Vec<String>,
    successes: Vec<String>,
}

/// Compare the declared `services:`/`tools:` maps in robot.yaml against the
/// Cargo workspace's own discovered runtime crates - pure string comparison,
/// no compilation. `project` is the same locked resolution [`Validate::run`]
/// already computed; declaration invariants and the resolution itself are the
/// caller's responsibility (see the doc comment on
/// [`declared_config_source_participants`] for why: both checks read the same
/// `project.runtimes`, so resolving it once and sharing it here avoids a
/// second `cargo metadata --locked` invocation).
fn workspace_runtime_report(robot: &Robot, project: &LockedProject) -> WorkspaceRuntimeReport {
    let mut report = WorkspaceRuntimeReport::default();
    let discovered = |kind: WorkspaceRuntimeKind| {
        project
            .runtimes
            .iter()
            .filter(move |runtime| runtime.kind == kind)
            .filter_map(|runtime| {
                runtime
                    .crate_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .collect::<BTreeSet<_>>()
    };
    let services = discovered(WorkspaceRuntimeKind::Service);
    let tool_crates = discovered(WorkspaceRuntimeKind::Tool);
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
    for configured in robot.tools.keys() {
        if !tool_crates.contains(configured) {
            report.problems.push(format!(
                "tools.{configured} has no matching tools/{configured} workspace crate"
            ));
        }
    }
    report
}

/// Every workspace runtime crate that can legally carry a robot.yaml `config:`
/// value: a `services/<id>` or `tools/<id>` crate whose `<id>` is a declared
/// key in `robot.services`/`robot.tools`. An official identity can never be a
/// declared key (`validate_runtime_declarations` rejects that at parse time),
/// so this filter alone is enough to exclude a workspace crate that path-
/// overrides an official service or tool - no separate "is this an official
/// package name" lookup is needed here the way full graph resolution needs
/// one. A component driver crate carries no `config:` in robot.yaml at all
/// and is filtered out by kind. This scoping is the point (organization#951
/// WS4 follow-up brief): `validate` must build ONLY the crates whose schema
/// it actually needs to read, not the whole component/driver/official graph
/// `build`/`run` resolve.
fn declared_config_source_participants(
    robot: &Robot,
    project: &LockedProject,
) -> Vec<SourceParticipant> {
    project
        .runtimes
        .iter()
        .filter_map(|runtime| {
            let name = runtime.crate_dir.file_name()?.to_str()?.to_string();
            match runtime.kind {
                WorkspaceRuntimeKind::Service if robot.services.contains_key(&name) => Some(
                    SourceParticipant::user_service(name, runtime.crate_dir.clone()),
                ),
                WorkspaceRuntimeKind::Tool if robot.tools.contains_key(&name) => Some(
                    SourceParticipant::user_tool(name, runtime.crate_dir.clone()),
                ),
                _ => None,
            }
        })
        .collect()
}

/// Build every declared config-bearing participant and validate its
/// robot.yaml config against its own emitted schema, through the shared check
/// engine (`crate::check::run_check_with_context`) `build`/`run`/`simulate`
/// already use - never a second, divergent validator. `build` is the
/// (expensive, real-compilation) builder for tests to fake; production always
/// passes a closure that runs a real `cargo build` scoped to one crate (see
/// [`Validate::run`]).
fn check_declared_configs(
    robot: &Robot,
    source_participants: &[SourceParticipant],
    build: impl FnMut(&SourceParticipant) -> Result<RawParticipantReport>,
) -> Result<CheckOutcome> {
    run_check_with_context(
        &[],
        &[],
        source_participants,
        CheckGraphContext { robot: Some(robot) },
        |image_ref| {
            bail!(
                "validate does not fetch official artifact reports (unexpected request for \
                 {image_ref})"
            )
        },
        |tool| {
            bail!(
                "validate does not fetch tool artifact reports (unexpected request for {})",
                tool.name
            )
        },
        build,
    )
}

fn print_text_report(robot: &Robot, train: &str) {
    println!("robot: {}", robot.robot.id);
    println!("train: {train}");
    println!("platform_services:");
    for official in catalog::NATIVE
        .iter()
        .filter(|official| official.kind == ArtifactKind::Service)
    {
        println!("  - {} -> {train}", official.package);
    }
    println!("services:");
    for name in robot.services.keys() {
        println!("  - {name} (declared)");
    }
    println!("tools:");
    for name in robot.tools.keys() {
        println!("  - {name} (declared)");
    }
    println!("components:");
    for (instance_name, instance) in &robot.robot.components {
        let driver = if instance.driver.is_some() {
            "driver"
        } else {
            "no-driver"
        };
        println!(
            "  - {} ({}) from {}",
            instance_name, driver, instance.component
        );
    }
}

fn print_json_report(robot: &Robot, train: &str) -> Result<()> {
    let report = serde_json::json!({
        "robot": robot.robot.id,
        "train": train,
        "platform_services": catalog::NATIVE.iter().filter(|official| official.kind == ArtifactKind::Service).map(|official| {
            serde_json::json!({
                "name": official.package,
                "version": train,
                "found": true,
            })
        }).collect::<Vec<_>>(),
        "services": robot.services.keys().map(|name| {
            serde_json::json!({
                "name": name,
                "declared": true,
            })
        }).collect::<Vec<_>>(),
        "tools": robot.tools.keys().map(|name| {
            serde_json::json!({
                "name": name,
                "declared": true,
            })
        }).collect::<Vec<_>>(),
        "components": robot.robot.components.iter().map(|(instance_name, instance)| {
            serde_json::json!({
                "instance": instance_name,
                "source": instance.component,
                "has_driver": instance.driver.is_some(),
            })
        }).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn join_errors(errors: Vec<phoxal::model::robot::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{RawArtifact, RawParticipantReport};
    use phoxal_cli_core::project::train::{LockedTrain, TrainSource, WorkspaceRuntime};
    use std::path::PathBuf;

    fn locked_project(runtimes: Vec<WorkspaceRuntime>) -> LockedProject {
        LockedProject {
            train: LockedTrain {
                version: "0.42.0".to_string(),
                source: TrainSource::Registry,
            },
            runtimes,
        }
    }

    fn workspace_runtime(dir: &str, kind: WorkspaceRuntimeKind) -> WorkspaceRuntime {
        let crate_dir = PathBuf::from(format!("/fake/project/{dir}"));
        let name = crate_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        WorkspaceRuntime {
            package: name.clone(),
            crate_dir,
            kind,
            binary_names: vec![name],
            component_assets: None,
        }
    }

    const MINIMAL_ROBOT: &str = r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
services:
  avoid: {}
tools:
  lidar-viz: {}
"#;

    fn minimal_robot() -> Robot {
        Robot::parse_from_string(MINIMAL_ROBOT).expect("minimal fixture robot.yaml parses")
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
            participant_class: "checked".to_string(),
            config_schema: Some(schema),
        }
    }

    #[test]
    fn workspace_runtime_report_flags_missing_and_notes_undeclared_crates() {
        let robot = minimal_robot();
        let project = locked_project(vec![
            workspace_runtime("services/avoid", WorkspaceRuntimeKind::Service),
            workspace_runtime("services/extra", WorkspaceRuntimeKind::Service),
        ]);
        let report = workspace_runtime_report(&robot, &project);
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("tools.lidar-viz")),
            "{report:?}"
        );
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
    fn declared_config_source_participants_covers_only_declared_services_and_tools() {
        let robot = minimal_robot();
        let project = locked_project(vec![
            workspace_runtime("services/avoid", WorkspaceRuntimeKind::Service),
            // Present on disk but not declared in robot.services - a drift
            // diagnostic elsewhere, never a config-check participant here.
            workspace_runtime("services/undeclared", WorkspaceRuntimeKind::Service),
            workspace_runtime("tools/lidar-viz", WorkspaceRuntimeKind::Tool),
            // A component crate never carries a robot.yaml `config:` value.
            workspace_runtime("components/ddsm115", WorkspaceRuntimeKind::Component),
        ]);

        let participants = declared_config_source_participants(&robot, &project);

        assert_eq!(participants.len(), 2, "{participants:?}");
        assert!(
            participants
                .iter()
                .any(|participant| participant.name == "avoid"
                    && participant.kind
                        == phoxal_cli_core::check::source::SourceParticipantKind::UserService)
        );
        assert!(
            participants
                .iter()
                .any(|participant| participant.name == "lidar-viz"
                    && participant.kind
                        == phoxal_cli_core::check::source::SourceParticipantKind::UserTool)
        );
    }

    /// The core end-to-end shape the maintainer asked for: a config that
    /// violates its participant's embedded schema must fail the check, and a
    /// valid one must pass, wired through the exact function `Validate::run`
    /// calls - `check_declared_configs` - with a fake (no `cargo`, no
    /// network) build closure standing in for the real compile, matching how
    /// `src/check/tests.rs` fakes the same seam.
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
