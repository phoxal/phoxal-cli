use clap::{CommandFactory, Parser};
use phoxal_cli::commands::{Cli, MessageFormat, RootCommand, deploy, robot, self_cmd, service};

#[test]
fn clap_definition_is_valid() {
    Cli::command().debug_assert();
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
fn parses_service_add() {
    let cli = Cli::try_parse_from(["phoxal-cli", "service", "add", "avoid-obstacles"])
        .expect("service add command should parse");

    let RootCommand::Service(command) = cli.command else {
        panic!("expected service command");
    };
    let service::ServiceSubcommand::Add(add) = command.command else {
        panic!("expected service add command");
    };

    assert_eq!(add.name, "avoid-obstacles");
}

#[test]
fn parses_service_run() {
    let cli = Cli::try_parse_from(["phoxal-cli", "service", "run", "avoid-obstacles"])
        .expect("service run command should parse");

    let RootCommand::Service(command) = cli.command else {
        panic!("expected service command");
    };
    let service::ServiceSubcommand::Run(run) = command.command else {
        panic!("expected service run command");
    };

    assert_eq!(run.name, "avoid-obstacles");
}

#[test]
fn parses_service_image_with_json_output() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "service",
        "image",
        "avoid-obstacles",
        "--message-format",
        "json",
    ])
    .expect("service image command should parse");

    let RootCommand::Service(command) = cli.command else {
        panic!("expected service command");
    };
    let service::ServiceSubcommand::Image(image) = command.command else {
        panic!("expected service image command");
    };

    assert_eq!(image.name.as_deref(), Some("avoid-obstacles"));
    assert_eq!(image.message_format, MessageFormat::Json);
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
    let service::ServiceSubcommand::Catalog(catalog) = command.command else {
        panic!("expected service catalog command");
    };

    assert_eq!(catalog.message_format, MessageFormat::Json);
}

#[test]
fn runtime_surface_is_removed() {
    assert!(Cli::try_parse_from(["phoxal-cli", "runtime", "add", "avoid-obstacles"]).is_err());
    assert!(Cli::try_parse_from(["phoxal-cli", "check", "--runtime", "avoid-obstacles",]).is_err());
}

#[test]
fn parses_check_pull_service_and_json_output() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "check",
        "--pull",
        "--service",
        "avoid-obstacles",
        "--message-format",
        "json",
    ])
    .expect("check command should parse");

    let RootCommand::Check(command) = cli.command else {
        panic!("expected check command");
    };

    assert!(command.pull);
    assert_eq!(command.service.as_deref(), Some("avoid-obstacles"));
    assert_eq!(command.message_format, MessageFormat::Json);
}

#[test]
fn parses_simulate_pull() {
    let cli = Cli::try_parse_from(["phoxal-cli", "simulate", "default", "--pull"])
        .expect("simulate --pull should parse");

    let RootCommand::Simulate(simulate) = cli.command else {
        panic!("expected simulate command");
    };

    assert!(simulate.pull);
}

#[test]
fn parses_deploy_build_defaults_to_compose() {
    let cli = Cli::try_parse_from(["phoxal-cli", "deploy", "build"])
        .expect("deploy build command should parse");

    let RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };
    let deploy::DeploySubcommand::Build(build) = command.command;

    assert_eq!(build.target, deploy::DeployTarget::Compose);
    assert!(build.output.is_none());
}

#[test]
fn parses_deploy_build_env_and_json_output() {
    let cli = Cli::try_parse_from([
        "phoxal-cli",
        "deploy",
        "build",
        "--env",
        "prod",
        "--message-format",
        "json",
    ])
    .expect("deploy build command should parse");

    let RootCommand::Deploy(command) = cli.command else {
        panic!("expected deploy command");
    };
    let deploy::DeploySubcommand::Build(build) = command.command;

    assert_eq!(build.env, vec!["prod"]);
    assert_eq!(build.message_format, MessageFormat::Json);
}

#[test]
fn parses_robot_new_pull_and_outdated() {
    let cli =
        Cli::try_parse_from(["phoxal-cli", "robot", "new", "rover"]).expect("robot new parses");
    let RootCommand::Robot(command) = cli.command else {
        panic!("expected robot command");
    };
    let robot::RobotSubcommand::New(new) = command.command;
    assert_eq!(new.name, "rover");

    let cli = Cli::try_parse_from(["phoxal-cli", "pull"]).expect("pull parses");
    assert!(matches!(cli.command, RootCommand::Pull(_)));

    let cli = Cli::try_parse_from(["phoxal-cli", "outdated"]).expect("outdated parses");
    assert!(matches!(cli.command, RootCommand::Outdated(_)));
}
