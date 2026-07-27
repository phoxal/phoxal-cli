//! Command-surface parsing tests: what the CLI accepts, and what it must keep
//! rejecting. These exercise the clap definition directly, without running a
//! command.

use super::{Cli, RootCommand, self_cmd, status};
use clap::{CommandFactory, Parser};

#[test]
fn clap_definition_is_valid() {
    Cli::command().debug_assert();
}

#[test]
fn parses_self_upgrade_with_normalized_version() {
    let cli = Cli::try_parse_from(["phoxal", "self", "upgrade", "--version", "v0.4.0"])
        .expect("self upgrade command should parse");
    let RootCommand::SelfCmd(command) = cli.command else {
        panic!("expected self command");
    };
    let self_cmd::SelfSubcommand::Upgrade(upgrade) = command.command;
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
        vec!["phoxal", "service", "suite", "--message-format", "json"],
        vec!["phoxal", "version", "--message-format", "json"],
        vec!["phoxal", "self", "upgrade", "--message-format", "json"],
        vec!["phoxal", "behavior", "validate", "--message-format", "json"],
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
fn rejects_watch_simulation_join_and_removed_overlays() {
    assert!(Cli::try_parse_from(["phoxal", "run", "--watch"]).is_err());
    assert!(Cli::try_parse_from(["phoxal", "run", "--env", "dev"]).is_err());

    for removed in [
        vec!["--watch"],
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
    let crate::simulation::SimulationSubcommand::Webots(webots) = simulation.command;
    let crate::simulation::WebotsSubcommand::Run(run) = webots.command;
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
    let crate::simulation::SimulationSubcommand::Webots(webots) = simulation.command;
    let crate::simulation::WebotsSubcommand::Run(run) = webots.command;
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

    // `start` is headless: no `--watch`, no `run`-only driver/detach flags.
    for rejected in [
        vec!["phoxal", "start", "--watch"],
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
        let cli = Cli::try_parse_from(["phoxal", "status", domain, "--connect", "tcp/robot:7447"])
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
