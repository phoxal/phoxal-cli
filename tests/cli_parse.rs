use clap::{CommandFactory, Parser};
use phoxal_cli::commands::{Cli, RootCommand, behavior, self_cmd, status};

#[test]
fn clap_definition_is_valid() {
    Cli::command().debug_assert();
}

#[test]
fn parses_behavior_validation_and_test_surface() {
    let cli = Cli::try_parse_from(["phoxal", "behavior", "validate"])
        .expect("behavior validate should parse");
    let RootCommand::Behavior(command) = cli.command else {
        panic!("expected behavior command");
    };
    assert!(matches!(
        command.command,
        behavior::BehaviorSubcommand::Validate(_)
    ));

    let cli = Cli::try_parse_from([
        "phoxal",
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
        vec!["phoxal", "check", "--message-format", "json"],
        vec!["phoxal", "run", "--message-format", "json"],
        vec![
            "phoxal",
            "simulation",
            "run",
            "default",
            "--message-format",
            "json",
        ],
        vec!["phoxal", "deploy", "--message-format", "json"],
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
        vec!["phoxal", "check", "--runtime", "avoid_obstacles"],
        vec!["phoxal", "simulate", "default"],
        vec!["phoxal", "simulation", "default"],
        vec!["phoxal", "simulation", "run", "default", "--pull"],
        vec!["phoxal", "check", "--pull"],
        vec!["phoxal", "validate", "--allow-user-service-drift"],
        vec!["phoxal", "deploy", "build"],
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
fn parses_watch_and_simulation_join_and_rejects_removed_overlays() {
    let cli = Cli::try_parse_from(["phoxal", "run", "--watch"]).expect("run watch should parse");
    let RootCommand::Run(run) = cli.command else {
        panic!("expected run command");
    };
    assert!(run.watch);
    assert!(Cli::try_parse_from(["phoxal", "run", "--env", "dev"]).is_err());

    let cli = Cli::try_parse_from(["phoxal", "simulation", "run", "default", "--watch"])
        .expect("simulation run watch should parse");
    let RootCommand::Simulation(simulation) = cli.command else {
        panic!("expected simulation command");
    };
    let phoxal_cli::simulation::SimulationSubcommand::Run(run) = simulation.command else {
        panic!("expected simulation run subcommand");
    };
    assert!(run.watch);
    assert!(
        Cli::try_parse_from(["phoxal", "simulation", "run", "default", "--env", "dev",]).is_err()
    );

    let cli = Cli::try_parse_from(["phoxal", "simulation", "join"])
        .expect("simulation join should parse");
    let RootCommand::Simulation(simulation) = cli.command else {
        panic!("expected simulation command");
    };
    assert!(matches!(
        simulation.command,
        phoxal_cli::simulation::SimulationSubcommand::Join(_)
    ));
}

#[test]
fn parses_bus_backed_status_commands() {
    assert!(Cli::try_parse_from(["phoxal", "status"]).is_err());
    assert!(Cli::try_parse_from(["phoxal", "status", "release", "mission"]).is_err());
    assert!(Cli::try_parse_from(["phoxal", "status", "resume", "mission"]).is_err());
    let cli = Cli::try_parse_from([
        "phoxal",
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

    let cli = Cli::try_parse_from(["phoxal", "status", "reset-estop"])
        .expect("status reset-estop should parse");
    let RootCommand::Status(command) = cli.command else {
        panic!("expected status command");
    };
    assert!(matches!(
        command.command,
        status::StatusSubcommand::ResetEstop(_)
    ));

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
            _ => panic!("expected domain-native status command for {domain}"),
        };
        assert_eq!(connect, "tcp/robot:7447");
    }
}

#[test]
fn parses_check_strict_and_rejects_removed_service_scope() {
    let cli =
        Cli::try_parse_from(["phoxal", "check", "--strict"]).expect("check command should parse");
    let RootCommand::Check(command) = cli.command else {
        panic!("expected check command");
    };
    assert!(command.strict);
    assert!(Cli::try_parse_from(["phoxal", "check", "--service", "avoid_obstacles",]).is_err());
}

#[test]
fn parses_deploy_and_update() {
    let cli = Cli::try_parse_from(["phoxal", "deploy", "robot@192.168.1.50"])
        .expect("deploy command should parse");
    let RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };
    assert_eq!(command.host.as_deref(), Some("robot@192.168.1.50"));
    assert!(Cli::try_parse_from(["phoxal", "deploy", "--env", "prod"]).is_err());

    let cli = Cli::try_parse_from(["phoxal", "deploy", "--dry-run", "--target", "aarch64"])
        .expect("deploy dry-run should parse");
    let RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };
    assert!(command.dry_run);
    assert_eq!(command.target.as_deref(), Some("aarch64"));

    assert!(Cli::try_parse_from(["phoxal", "update", "--dry-run"]).is_ok());
    assert!(Cli::try_parse_from(["phoxal", "--plain", "check"]).is_err());
    assert!(Cli::try_parse_from(["phoxal", "check", "--plain"]).is_err());
    assert!(Cli::try_parse_from(["phoxal", "--quiet", "check"]).is_err());
    assert!(Cli::try_parse_from(["phoxal", "--welcome", "check"]).is_err());
}
