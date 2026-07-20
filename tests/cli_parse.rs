use clap::{CommandFactory, Parser};
use phoxal_cli::commands::{Cli, RootCommand, behavior, self_cmd, status};

#[test]
fn clap_definition_is_valid() {
    Cli::command().debug_assert();
}

#[test]
fn parses_behavior_validation_and_test_surface() {
    let cli = Cli::try_parse_from(["phoxal-cli", "behavior", "validate"])
        .expect("behavior validate should parse");
    let RootCommand::Behavior(command) = cli.command else {
        panic!("expected behavior command");
    };
    assert!(matches!(
        command.command,
        behavior::BehaviorSubcommand::Validate(_)
    ));

    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "behavior",
        "test",
        "navigation.return_to_dock",
        "--arg",
        "dock=home",
        "--scenario",
        "timeout",
    ])
    .expect("behavior test should parse");
    let RootCommand::Behavior(command) = cli.command else {
        panic!("expected behavior command");
    };
    let behavior::BehaviorSubcommand::Test(test) = command.command else {
        panic!("expected test subcommand");
    };
    assert_eq!(test.behavior_id, "navigation.return_to_dock");
    assert_eq!(test.args, vec!["dock=home"]);
    assert_eq!(test.scenario, "timeout");
}

#[test]
fn parses_self_upgrade_with_normalized_version() {
    let cli = Cli::try_parse_from(["phoxal-cli", "self", "upgrade", "--version", "v0.4.0"])
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
        vec!["phoxal-cli", "check", "--message-format", "json"],
        vec!["phoxal-cli", "run", "--message-format", "json"],
        vec![
            "phoxal-cli",
            "simulation",
            "run",
            "default",
            "--message-format",
            "json",
        ],
        vec!["phoxal-cli", "deploy", "--message-format", "json"],
        vec!["phoxal-cli", "update", "--message-format", "json"],
        vec!["phoxal-cli", "status", "--message-format", "json", "safety"],
        vec![
            "phoxal-cli",
            "service",
            "catalog",
            "--message-format",
            "json",
        ],
        vec!["phoxal-cli", "version", "--message-format", "json"],
        vec!["phoxal-cli", "self", "upgrade", "--message-format", "json"],
        vec![
            "phoxal-cli",
            "behavior",
            "validate",
            "--message-format",
            "json",
        ],
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
        vec!["phoxal-cli", "service", "add", "avoid_obstacles"],
        vec!["phoxal-cli", "service", "run", "avoid_obstacles"],
        vec!["phoxal-cli", "runtime", "add", "avoid_obstacles"],
        vec!["phoxal-cli", "check", "--runtime", "avoid_obstacles"],
        vec!["phoxal-cli", "simulate", "default"],
        vec!["phoxal-cli", "simulation", "default"],
        vec!["phoxal-cli", "simulation", "run", "default", "--pull"],
        vec!["phoxal-cli", "check", "--pull"],
        vec!["phoxal-cli", "deploy", "build"],
        vec!["phoxal-cli", "robot", "new", "rover"],
        vec!["phoxal-cli", "robot"],
        vec!["phoxal-cli", "pull"],
        vec!["phoxal-cli", "outdated"],
        vec!["phoxal-cli", "cache"],
        vec!["phoxal-cli", "cache", "clean"],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn parses_watch_overlay_and_simulation_join() {
    let cli = Cli::try_parse_from(["phoxal-cli", "run", "--watch", "--env", "dev"])
        .expect("run watch should parse");
    let RootCommand::Run(run) = cli.command else {
        panic!("expected run command");
    };
    assert!(run.watch);
    assert_eq!(run.env, vec!["dev"]);

    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "simulation",
        "run",
        "default",
        "--watch",
        "--env",
        "dev",
    ])
    .expect("simulation run watch should parse");
    let RootCommand::Simulation(simulation) = cli.command else {
        panic!("expected simulation command");
    };
    let phoxal_cli::commands::simulate::SimulationSubcommand::Run(run) = simulation.command else {
        panic!("expected simulation run subcommand");
    };
    assert!(run.watch);
    assert_eq!(run.env, vec!["dev"]);

    let cli = Cli::try_parse_from(["phoxal-cli", "simulation", "join"])
        .expect("simulation join should parse");
    let RootCommand::Simulation(simulation) = cli.command else {
        panic!("expected simulation command");
    };
    assert!(matches!(
        simulation.command,
        phoxal_cli::commands::simulate::SimulationSubcommand::Join(_)
    ));
}

#[test]
fn parses_bus_backed_status_commands() {
    assert!(Cli::try_parse_from(["phoxal-cli", "status"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "status", "release", "mission"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "status", "resume", "mission"]).is_err());
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "status",
        "engage-estop",
        "--connect",
        "tcp/robot:7447",
    ])
    .expect("status engage-estop should parse");
    let RootCommand::Status(command) = cli.command else {
        panic!("expected status command");
    };
    let status::StatusSubcommand::EngageEstop(arg) = command.command else {
        panic!("expected engage-estop subcommand");
    };
    assert_eq!(arg.connect, "tcp/robot:7447");

    let cli = Cli::try_parse_from(["phoxal-cli", "status", "reset-estop"])
        .expect("status reset-estop should parse");
    let RootCommand::Status(command) = cli.command else {
        panic!("expected status command");
    };
    assert!(matches!(
        command.command,
        status::StatusSubcommand::ResetEstop(_)
    ));

    for domain in ["safety", "motion", "localization"] {
        let cli = Cli::try_parse_from([
            "phoxal-cli",
            "status",
            domain,
            "--connect",
            "tcp/robot:7447",
        ])
        .unwrap_or_else(|error| panic!("status {domain} should parse: {error}"));
        let RootCommand::Status(command) = cli.command else {
            panic!("expected status command for {domain}");
        };
        let connect = match command.command {
            status::StatusSubcommand::Safety(arg)
            | status::StatusSubcommand::Motion(arg)
            | status::StatusSubcommand::Localization(arg) => arg.connect,
            _ => panic!("expected domain-native status command for {domain}"),
        };
        assert_eq!(connect, "tcp/robot:7447");
    }
}

#[test]
fn parses_check_service_and_strict() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "check",
        "--service",
        "avoid_obstacles",
        "--strict",
    ])
    .expect("check command should parse");
    let RootCommand::Check(command) = cli.command else {
        panic!("expected check command");
    };
    assert_eq!(command.service.as_deref(), Some("avoid_obstacles"));
    assert!(command.strict);
}

#[test]
fn parses_deploy_and_update() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "deploy",
        "robot@192.168.1.50",
        "--env",
        "prod",
    ])
    .expect("deploy command should parse");
    let RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };
    assert_eq!(command.host.as_deref(), Some("robot@192.168.1.50"));
    assert_eq!(command.env, vec!["prod"]);

    let cli = Cli::try_parse_from(["phoxal-cli", "deploy", "--dry-run", "--target", "aarch64"])
        .expect("deploy dry-run should parse");
    let RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };
    assert!(command.dry_run);
    assert_eq!(command.target.as_deref(), Some("aarch64"));

    assert!(Cli::try_parse_from(["phoxal-cli", "update", "--dry-run"]).is_ok());
    assert!(Cli::try_parse_from(["phoxal-cli", "--plain", "check"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "check", "--plain"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "--quiet", "check"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "--welcome", "check"]).is_err());
}
