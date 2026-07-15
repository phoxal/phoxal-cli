use clap::{CommandFactory, Parser};
use phoxal_cli::commands::{Cli, MessageFormat, RootCommand, behavior, self_cmd, service, status};

#[test]
fn clap_definition_is_valid() {
    Cli::command().debug_assert();
}

#[test]
fn parses_behavior_validation_and_test_surface() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "behavior",
        "validate",
        "--message-format",
        "json",
    ])
    .expect("behavior validate should parse");
    let RootCommand::Behavior(command) = cli.command else {
        panic!("expected behavior command");
    };
    let behavior::BehaviorSubcommand::Validate(validate) = command.command else {
        panic!("expected validate subcommand");
    };
    assert_eq!(validate.message_format, MessageFormat::Json);

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
    assert_eq!(upgrade.message_format, MessageFormat::Human);
}

#[test]
fn parses_self_upgrade_json_output() {
    let cli = Cli::try_parse_from(["phoxal-cli", "self", "upgrade", "--message-format", "json"])
        .expect("self upgrade command should parse");

    let RootCommand::SelfCmd(command) = cli.command else {
        panic!("expected self command");
    };
    let self_cmd::SelfSubcommand::Upgrade(upgrade) = command.command;

    assert_eq!(upgrade.message_format, MessageFormat::Json);
}

#[test]
fn parses_version_human_default_and_json_output() {
    let cli = Cli::try_parse_from(["phoxal-cli", "version"]).expect("version command should parse");
    let RootCommand::Version(command) = cli.command else {
        panic!("expected version command");
    };
    assert_eq!(command.message_format, MessageFormat::Human);

    let cli = Cli::try_parse_from(["phoxal-cli", "version", "--message-format", "json"])
        .expect("version --message-format json should parse");
    let RootCommand::Version(command) = cli.command else {
        panic!("expected version command");
    };
    assert_eq!(command.message_format, MessageFormat::Json);
}

#[test]
fn service_add_is_removed() {
    assert!(Cli::try_parse_from(["phoxal-cli", "service", "add", "avoid_obstacles"]).is_err());
}

#[test]
fn service_run_is_removed() {
    assert!(Cli::try_parse_from(["phoxal-cli", "service", "run", "avoid_obstacles"]).is_err());
}

#[test]
fn parses_service_catalog_json_output() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "service",
        "catalog",
        "--message-format",
        "json",
    ])
    .expect("service catalog command should parse");

    let RootCommand::Service(command) = cli.command else {
        panic!("expected service command");
    };
    let service::ServiceSubcommand::Catalog(catalog) = command.command;

    assert_eq!(catalog.message_format, MessageFormat::Json);
}

#[test]
fn runtime_surface_is_removed() {
    assert!(Cli::try_parse_from(["phoxal-cli", "runtime", "add", "avoid_obstacles"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "check", "--runtime", "avoid_obstacles",]).is_err());
}

#[test]
fn parses_check_service_and_json_output_and_rejects_pull() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "check",
        "--service",
        "avoid_obstacles",
        "--message-format",
        "json",
        "--strict",
    ])
    .expect("check command should parse");

    let RootCommand::Check(command) = cli.command else {
        panic!("expected check command");
    };

    assert_eq!(command.service.as_deref(), Some("avoid_obstacles"));
    assert_eq!(command.message_format, MessageFormat::Json);
    assert!(command.strict);
    assert!(Cli::try_parse_from(["phoxal-cli", "check", "--pull"]).is_err());
}

#[test]
fn simulation_run_pull_is_removed() {
    assert!(Cli::try_parse_from(["phoxal-cli", "simulation", "run", "default", "--pull"]).is_err());
}

#[test]
fn bare_simulate_verb_is_gone() {
    // Clean cut (Phase 1b): no `simulate` alias, no bare `simulation <world>`
    // shorthand - only `simulation run <world>` / `simulation join`.
    assert!(Cli::try_parse_from(["phoxal-cli", "simulate", "default"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "simulation", "default"]).is_err());
}

#[test]
fn parses_simulation_join_stub() {
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
fn parses_watch_and_overlay_flags() {
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
}

#[test]
fn parses_status_release_resume_and_json() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "status",
        "--message-format",
        "json",
        "release",
        "mission",
    ])
    .expect("status release should parse");
    let RootCommand::Status(command) = cli.command else {
        panic!("expected status command");
    };
    assert_eq!(command.message_format, MessageFormat::Json);
    let Some(status::StatusSubcommand::Release(arg)) = command.command else {
        panic!("expected release subcommand");
    };
    assert_eq!(arg.participant, "mission");

    let cli = Cli::try_parse_from(["phoxal-cli", "status", "resume", "mission"])
        .expect("status resume should parse");
    let RootCommand::Status(command) = cli.command else {
        panic!("expected status command");
    };
    let Some(status::StatusSubcommand::Resume(arg)) = command.command else {
        panic!("expected resume subcommand");
    };
    assert_eq!(arg.participant, "mission");

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
    let Some(status::StatusSubcommand::EngageEstop(arg)) = command.command else {
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
        Some(status::StatusSubcommand::ResetEstop(_))
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
            Some(status::StatusSubcommand::Safety(arg))
            | Some(status::StatusSubcommand::Motion(arg))
            | Some(status::StatusSubcommand::Localization(arg)) => arg.connect,
            _ => panic!("expected domain-native status command for {domain}"),
        };
        assert_eq!(connect, "tcp/robot:7447");
    }
}

#[test]
fn parses_deploy_single_verb() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "deploy",
        "robot@192.168.1.50",
        "--env",
        "prod",
        "--message-format",
        "json",
    ])
    .expect("deploy command should parse");

    let RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };

    assert_eq!(command.host.as_deref(), Some("robot@192.168.1.50"));
    assert_eq!(command.env, vec!["prod"]);
    assert_eq!(command.message_format, MessageFormat::Json);
}

#[test]
fn parses_deploy_dry_run_target_and_removes_build_pair() {
    let cli = Cli::try_parse_from(["phoxal-cli", "deploy", "--dry-run", "--target", "aarch64"])
        .expect("deploy dry-run should parse");

    let RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };

    assert!(command.dry_run);
    assert_eq!(command.target.as_deref(), Some("aarch64"));
    assert!(Cli::try_parse_from(["phoxal-cli", "deploy", "build"]).is_err());
}

#[test]
fn robot_new_is_removed() {
    assert!(Cli::try_parse_from(["phoxal-cli", "robot", "new", "rover"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "robot"]).is_err());
}

#[test]
fn parses_update_and_rejects_removed_commands() {
    let cli = Cli::try_parse_from(["phoxal-cli", "update", "--dry-run"]).expect("update parses");
    assert!(matches!(cli.command, RootCommand::Update(_)));
    assert!(Cli::try_parse_from(["phoxal-cli", "pull"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "outdated"]).is_err());
}

#[test]
fn parses_plain_as_a_global_flag() {
    let cli = Cli::try_parse_from(["phoxal-cli", "--plain", "check"])
        .expect("global --plain should parse before a verb");
    assert!(cli.plain);

    let cli = Cli::try_parse_from(["phoxal-cli", "check", "--plain"])
        .expect("--plain also parses after the verb (global = true)");
    assert!(cli.plain);
}

#[test]
fn welcome_is_no_longer_a_flag() {
    // Product decision 4: the welcome card is always the default human
    // rendering now - there is no `--welcome` flag left to parse.
    assert!(Cli::try_parse_from(["phoxal-cli", "--welcome", "check"]).is_err());
}

#[test]
fn parses_cache_clean_and_dry_run() {
    let cli = Cli::try_parse_from(["phoxal-cli", "cache", "clean"]).expect("cache clean parses");
    let RootCommand::Cache(cache) = cli.command else {
        panic!("expected cache command");
    };
    let phoxal_cli::commands::cache::CacheSubcommand::Clean(clean) = cache.command;
    assert!(!clean.dry_run);

    let cli = Cli::try_parse_from(["phoxal-cli", "cache", "clean", "--dry-run"])
        .expect("cache clean --dry-run parses");
    let RootCommand::Cache(cache) = cli.command else {
        panic!("expected cache command");
    };
    let phoxal_cli::commands::cache::CacheSubcommand::Clean(clean) = cache.command;
    assert!(clean.dry_run);
}
