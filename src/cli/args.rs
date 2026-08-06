//! Command-line argument surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use super::commands::{
    attach, build, deploy, doctor, install, logs, rollback, run, schema, self_update, service,
    simulation, start, status, stop, validate, version,
};

#[derive(Debug, Parser)]
#[command(
    name = "phoxal",
    version = version::long_version(),
    about = "Build, check, and simulate Phoxal robot projects.",
    long_about = "Build, check, and simulate Phoxal robot projects.\n\n\
                  phoxal reads robot.yaml and materializes official services and component drivers with `cargo install` against the phoxal registry, pinned exactly to the Cargo.lock-selected framework train, then drives the develop/simulate loop. Start by hand-authoring robot.yaml (see the framework repo's examples/ and getting-started docs), then run `build`, `run`, or `simulation webots run` - each validates the graph and every participant's config before it executes.\n\n\
                  Every robot project's ROOT Cargo package is its one mandatory brain: a non-published workspace member depending on `phoxal`, with exactly one binary target and no library. The minimal root source is:\n\n\
                  \x20 // src/main.rs\n\
                  \x20 use phoxal::prelude::*;\n\
                  \x20 #[phoxal::brain]\n\
                  \x20 struct Brain;\n\
                  \x20 impl Participant for Brain {\n\
                  \x20     async fn setup(&self, _ctx: &mut SetupContext<Self>, _config: Self::Config)\n\
                  \x20         -> Result<(Self::State, Self::Api)> { Ok(((), ())) }\n\
                  \x20 }\n\
                  \x20 fn main() -> phoxal::Result<()> { phoxal::run::<Brain>() }\n\n\
                  The CLI discovers it from Cargo metadata, always builds it, stages it as `bin/brain`, and launches it in every native and Webots graph. It is never declared under robot.yaml `services:` - `brain` is a reserved identity there. A project whose root is still a code-less `src/lib.rs` anchor is rejected with the exact migration instruction."
)]
pub struct Cli {
    #[arg(
        long = "project-path",
        global = true,
        help = "Project path used for robot.yaml discovery. Defaults to current directory."
    )]
    pub project_path: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Pass --offline to every cargo install/metadata invocation this command makes (also PHOXAL_OFFLINE)."
    )]
    pub offline: bool,
    #[command(subcommand)]
    pub command: RootCommand,
}

impl Cli {
    #[must_use]
    pub fn offline(&self) -> bool {
        self.offline || crate::cli::context::offline_from_env()
    }
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    #[command(
        about = "Stage a runtime layout for a target and archive it as build.phoxal.",
        long_about = "Stage a runtime layout for a target and archive it as a deterministic build.phoxal.\n\n\
                      `build` refreshes staging exactly as `run` would - but for the selected --target rather than the host - validates the staged layout through the shared loader against the declared target architecture (no execution), and archives the staged layout deterministically: identical contents always produce identical archive bytes. The default output is a sibling of the staged directory, <project>/.phoxal/<triple>.build.phoxal, and the path plus its sha256 are printed at the end.\n\n\
                      `--builder` selects where compilation happens, never a different output: `local` (the default) compiles on this host with `cargo build --target`; `container` compiles natively inside the pinned official rust image for the target platform; `ssh://user@host` snapshots the source, compiles in a remote temporary directory, and pulls back the same archive. Every backend produces the identical deterministic build.phoxal."
    )]
    Build(build::Build),
    #[command(about = "Build and install a runtime on a prepared robot over SSH.")]
    Deploy(deploy::Deploy),
    #[command(about = "Install one compiled runtime archive atomically.")]
    Install(install::Install),
    #[command(about = "Activate an older installed runtime release.")]
    Rollback(rollback::Rollback),
    #[command(
        about = "Validate robot.yaml structure, the root brain, Cargo workspace runtime ownership, and declared service config.",
        long_about = "Validate that this project is well-formed: robot.yaml structure, the mandatory root brain, Cargo workspace runtime ownership (every declared services entry has a matching workspace crate), and every declared service's config against the JSON Schema its own participant type embeds.\n\n\
                      The check compiles the root brain and the declared service crates (never the official set, never a staged bundle) to read their embedded metadata and schemas - the one part of `validate` that is not free. The root brain is always compiled, so its declared id, kind, and unit config schema are proven here."
    )]
    Validate(validate::Validate),
    #[command(about = "Generate portable JSON Schemas for authored YAML editors.")]
    Schema(schema::Schema),
    #[command(about = "Simulate a robot with `simulation webots run`.")]
    Simulation(simulation::Simulation),
    #[command(about = "Run the resolved robot graph with the host-native supervisor.")]
    Run(run::Run),
    #[command(
        about = "Run a robot instance headless (no TUI); the systemd-facing verb.",
        long_about = "Run a robot instance headless, with no terminal UI - the verb `phoxal.service` invokes.\n\n\
                      Uses the same universal pipeline as `run`: it classifies the root, refreshes staging when it is a buildable source project, and supervises the staged runtime layout. Invoked interactively it behaves like `run -d` - it detaches the resident supervisor, returns once required participants are ready, and prints how to attach or stop. Invoked under systemd (`Type=notify`) it stays the in-process foreground resident, signalling `READY=1` after readiness and pinging the watchdog while it runs."
    )]
    Start(start::Start),
    #[command(about = "Attach a thin terminal client to a running project supervisor.")]
    Attach(attach::Attach),
    #[command(about = "Orderly stop a running project supervisor.")]
    Stop(stop::Stop),
    #[command(about = "Stream participant bus logs from a reachable robot.")]
    Logs(logs::Logs),
    #[command(about = "Inspect live robot state through typed bus contracts.")]
    Status(status::Status),
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Manage the systemd phoxal.service.")]
    Service(service::Service),
    #[command(about = "Print the phoxal version and the official registry it installs from.")]
    Version(version::VersionArgs),
    #[command(name = "self", about = "Manage this phoxal installation.")]
    SelfCmd(self_update::SelfCmd),
}

#[cfg(test)]
mod tests {
    //! Command-surface parsing tests: what the CLI accepts, and what it must keep
    //! rejecting. These exercise the clap definition directly, without running a
    //! command.

    use super::super::commands::{self_update, status};
    use super::{Cli, RootCommand};
    use clap::{CommandFactory, Parser};

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_top_level_subcommand_has_help_text() {
        for command in Cli::command().get_subcommands() {
            assert!(
                command.get_about().is_some(),
                "top-level command `{}` must have help text",
                command.get_name()
            );
        }
    }

    #[test]
    fn build_command_parses_its_full_flag_surface() {
        let cli = Cli::try_parse_from([
            "phoxal",
            "build",
            "project",
            "--target",
            "aarch64-unknown-linux-gnu",
            "--builder",
            "container",
            "--output",
            "out/bundle.build.phoxal",
            "--container-engine",
            "podman",
            "--builder-image",
            "ghcr.io/example/custom:latest",
        ])
        .unwrap();
        let RootCommand::Build(build) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(
            build.project.as_deref(),
            Some(std::path::Path::new("project"))
        );
        assert_eq!(build.target.as_deref(), Some("aarch64-unknown-linux-gnu"));
        assert_eq!(build.builder, "container");
        assert_eq!(
            build.output.as_deref(),
            Some(std::path::Path::new("out/bundle.build.phoxal"))
        );
        assert_eq!(
            build.container_engine,
            super::super::commands::build::ContainerEngine::Podman
        );
        assert_eq!(
            build.builder_image.as_deref(),
            Some("ghcr.io/example/custom:latest")
        );
    }

    #[test]
    fn build_command_accepts_the_ssh_builder_string() {
        let cli =
            Cli::try_parse_from(["phoxal", "build", "--builder", "ssh://dev@jetson-nano-orin"])
                .unwrap();
        let RootCommand::Build(build) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(build.builder, "ssh://dev@jetson-nano-orin");
    }

    #[test]
    fn execution_id_round_trips_through_the_flag() {
        let execution = phoxal_cli_core::identity::ExecutionId::mint();
        let cli = Cli::try_parse_from([
            "phoxal",
            "status",
            "safety",
            "--execution",
            &execution.to_string(),
        ])
        .unwrap();
        let RootCommand::Status(status) = cli.command else {
            panic!("expected status")
        };
        let super::super::commands::status::StatusSubcommand::Safety(target) = status.command
        else {
            panic!("expected safety")
        };
        assert_eq!(target.execution, Some(execution));
    }

    #[test]
    fn parses_self_upgrade_with_normalized_version() {
        let cli = Cli::try_parse_from(["phoxal", "self", "upgrade", "--version", "v0.4.0"])
            .expect("self upgrade command should parse");
        let RootCommand::SelfCmd(command) = cli.command else {
            panic!("expected self command");
        };
        let self_update::SelfSubcommand::Upgrade(upgrade) = command.command;
        assert_eq!(
            upgrade.version.expect("version should parse").to_string(),
            "0.4.0"
        );
    }

    #[test]
    fn message_format_is_removed_from_every_former_surface() {
        for args in [
            vec!["phoxal", "run", "--message-format", "json"],
            vec![
                "phoxal",
                "simulation",
                "webots",
                "run",
                "default",
                "--message-format",
                "json",
            ],
            vec!["phoxal", "update", "--message-format", "json"],
            vec!["phoxal", "status", "--message-format", "json", "safety"],
            vec!["phoxal", "version", "--message-format", "json"],
            vec!["phoxal", "self", "upgrade", "--message-format", "json"],
        ] {
            assert!(
                Cli::try_parse_from(args.clone()).is_err(),
                "removed message-format flag unexpectedly parsed: {args:?}"
            );
        }
    }

    #[test]
    fn removed_command_surfaces_stay_removed() {
        for args in [
            vec!["phoxal", "service", "add", "avoid_obstacles"],
            vec!["phoxal", "service", "run", "avoid_obstacles"],
            // `service suite` is deleted: the suite descriptor it printed is
            // gone, and the catalog is no longer a per-project inventory.
            vec!["phoxal", "service", "suite"],
            vec!["phoxal", "runtime", "add", "avoid_obstacles"],
            vec!["phoxal", "check", "avoid_obstacles"],
            vec!["phoxal", "simulate", "default"],
            vec!["phoxal", "simulation", "default"],
            vec!["phoxal", "simulation", "webots", "run", "default", "--pull"],
            vec!["phoxal", "validate", "--allow-user-service-drift"],
            vec!["phoxal", "deploy", "--dry-run", "--target", "aarch64"],
            // `phoxal update` is deleted (organization#951 WS4): materialization
            // is Cargo's job now, not a project-vendoring command.
            vec!["phoxal", "update"],
            vec!["phoxal", "update", "--dry-run"],
            vec!["phoxal", "robot", "new", "rover"],
            vec!["phoxal", "robot"],
            vec!["phoxal", "pull"],
            vec!["phoxal", "outdated"],
            vec!["phoxal", "cache"],
            vec!["phoxal", "cache", "clean"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn parses_install_rollback_deploy_and_service_management() {
        assert!(matches!(
            Cli::try_parse_from(["phoxal", "install", "robot.build.phoxal"])
                .unwrap()
                .command,
            RootCommand::Install(_)
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "phoxal",
                "rollback",
                "--to",
                "20260725T010000.000Z-deadbeef"
            ])
            .unwrap()
            .command,
            RootCommand::Rollback(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["phoxal", "deploy", "robot@192.168.1.50"])
                .unwrap()
                .command,
            RootCommand::Deploy(_)
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "phoxal",
                "deploy",
                "robot@192.168.1.50",
                "--build",
                "robot.build.phoxal",
            ])
            .unwrap()
            .command,
            RootCommand::Deploy(_)
        ));
        for subcommand in ["install", "uninstall", "status"] {
            assert!(
                Cli::try_parse_from(["phoxal", "service", subcommand]).is_ok(),
                "service {subcommand} should parse"
            );
        }
    }

    #[test]
    fn rejects_removed_run_and_simulation_options_and_alternate_surfaces() {
        assert!(Cli::try_parse_from(["phoxal", "run", "--env", "dev"]).is_err());

        for removed in [
            vec!["--env", "dev"],
            vec!["--dry-run"],
            vec!["--target", "aarch64-apple-darwin"],
            vec!["--connect", "tcp/localhost:7447"],
            vec!["--no-drivers"],
            vec!["--external-router"],
        ] {
            let mut args = vec!["phoxal", "simulation", "webots", "run", "default"];
            args.extend(removed);
            assert!(
                Cli::try_parse_from(args.clone()).is_err(),
                "removed simulation option unexpectedly parsed: {args:?}"
            );
        }
        assert!(Cli::try_parse_from(["phoxal", "simulation", "run", "default"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "simulation", "join"]).is_err());
    }

    #[test]
    fn parses_the_single_webots_run_surface() {
        let cli = Cli::try_parse_from([
            "phoxal",
            "simulation",
            "webots",
            "run",
            "default",
            "--project",
            "/tmp/robot.yaml",
            "--detach",
        ])
        .expect("the approved Webots run surface should parse");
        let RootCommand::Simulation(simulation) = cli.command else {
            panic!("expected simulation command");
        };
        let crate::cli::commands::simulation::SimulationSubcommand::Webots(webots) =
            simulation.command;
        let crate::cli::commands::simulation::WebotsSubcommand::Run(run) = webots.command;
        assert_eq!(run.world, "default");
        assert_eq!(
            run.project.as_deref(),
            Some(std::path::Path::new("/tmp/robot.yaml"))
        );
        assert!(run.detach);

        let short = Cli::try_parse_from(["phoxal", "simulation", "webots", "run", "default", "-d"])
            .expect("-d should parse");
        let RootCommand::Simulation(simulation) = short.command else {
            panic!("expected simulation command");
        };
        let crate::cli::commands::simulation::SimulationSubcommand::Webots(webots) =
            simulation.command;
        let crate::cli::commands::simulation::WebotsSubcommand::Run(run) = webots.command;
        assert!(run.detach);
    }

    #[test]
    fn parses_start_and_rejects_interactive_flags() {
        let cli = Cli::try_parse_from(["phoxal", "start"]).expect("start should parse");
        assert!(matches!(cli.command, RootCommand::Start(_)));

        let cli = Cli::try_parse_from(["phoxal", "start", "/var/phoxal"])
            .expect("start with an explicit root should parse");
        let RootCommand::Start(start) = cli.command else {
            panic!("expected start command");
        };
        assert_eq!(
            start.target.as_deref(),
            Some(std::path::Path::new("/var/phoxal"))
        );

        // `start` is headless: it accepts no `run`-only driver/detach flags.
        for rejected in [
            vec!["phoxal", "start", "-d"],
            vec!["phoxal", "start", "--drivers", "off"],
            vec!["phoxal", "start", "--driver", "left_drive"],
        ] {
            assert!(
                Cli::try_parse_from(rejected.clone()).is_err(),
                "start unexpectedly accepted {rejected:?}"
            );
        }
    }

    #[test]
    fn parses_bus_backed_status_commands() {
        assert!(Cli::try_parse_from(["phoxal", "status"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "status", "release", "mission"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "status", "resume", "mission"]).is_err());
        // There is no software emergency-stop command: every emergency stop is a
        // manifest-declared component under the ordinary rules (#952 section A).
        for removed in ["engage-estop", "reset-estop"] {
            assert!(
                Cli::try_parse_from(["phoxal", "status", removed]).is_err(),
                "`status {removed}` must no longer parse"
            );
        }

        for domain in ["safety", "motion", "localization"] {
            let cli =
                Cli::try_parse_from(["phoxal", "status", domain, "--connect", "tcp/robot:7447"])
                    .unwrap_or_else(|error| panic!("status {domain} should parse: {error}"));
            let RootCommand::Status(command) = cli.command else {
                panic!("expected status command for {domain}");
            };
            let connect = match command.command {
                status::StatusSubcommand::Safety(arg)
                | status::StatusSubcommand::Motion(arg)
                | status::StatusSubcommand::Localization(arg) => arg.connect,
            };
            assert_eq!(connect, "tcp/robot:7447");
        }
    }

    #[test]
    fn rejects_unknown_global_flags() {
        assert!(Cli::try_parse_from(["phoxal", "--plain", "version"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "version", "--plain"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "--quiet", "version"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "--welcome", "version"]).is_err());
    }
}
