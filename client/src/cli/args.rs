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
        env = "PHOXAL_OFFLINE",
        help = "Pass --offline to every cargo install/metadata invocation this command makes."
    )]
    pub offline: bool,
    #[command(subcommand)]
    pub command: RootCommand,
}

impl Cli {
    #[must_use]
    pub const fn offline(&self) -> bool {
        self.offline
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
    //! Surface-wide invariants only. What one command accepts is that
    //! command's own test, beside the definition it is about.

    use super::Cli;
    use clap::CommandFactory;

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
    fn rejects_unknown_global_flags() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["phoxal", "--plain", "version"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "version", "--plain"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "--quiet", "version"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "--welcome", "version"]).is_err());
    }

    /// The whole private-IPC and resident vocabulary is gone: no verb, flag, or
    /// environment variable may reintroduce it (organization#978).
    #[test]
    fn no_resident_or_private_bootstrap_surface_survives() {
        use clap::Parser;
        for removed in [
            vec!["phoxal", "resident"],
            vec!["phoxal", "run", "--detach"],
            vec!["phoxal", "run", "-d"],
            vec!["phoxal", "simulation", "webots", "run", "default", "-d"],
            vec!["phoxal", "stop", "--force"],
        ] {
            assert!(
                Cli::try_parse_from(removed.clone()).is_err(),
                "removed surface unexpectedly parsed: {removed:?}"
            );
        }
    }

    #[test]
    fn removed_command_surfaces_stay_removed() {
        use clap::Parser;
        for args in [
            vec!["phoxal", "service", "add", "avoid_obstacles"],
            vec!["phoxal", "service", "run", "avoid_obstacles"],
            vec!["phoxal", "service", "suite"],
            vec!["phoxal", "runtime", "add", "avoid_obstacles"],
            vec!["phoxal", "check", "avoid_obstacles"],
            vec!["phoxal", "simulate", "default"],
            vec!["phoxal", "simulation", "default"],
            vec!["phoxal", "simulation", "run", "default"],
            vec!["phoxal", "validate", "--allow-user-service-drift"],
            vec!["phoxal", "deploy", "--dry-run", "--target", "aarch64"],
            vec!["phoxal", "update"],
            vec!["phoxal", "robot", "new", "rover"],
            vec!["phoxal", "pull"],
            vec!["phoxal", "outdated"],
            vec!["phoxal", "cache"],
        ] {
            assert!(Cli::try_parse_from(args.clone()).is_err(), "{args:?}");
        }
    }
}
