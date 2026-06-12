use clap::{CommandFactory, Parser};
use phoxal_cli::commands::{Cli, RootCommand, self_cmd};

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
