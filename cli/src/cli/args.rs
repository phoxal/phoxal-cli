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
                  The CLI discovers it from Cargo metadata, always builds it, stages it as `bin/brain`, and launches it in every native and Webots graph. It is never declared under robot.yaml `services:` - `brain` is a reserved identity there."
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
        about = "Stage a deployment release for a target and archive it as build.phoxal.",
        long_about = "Stage a deployment release for a target and archive it as a deterministic build.phoxal.\n\n\
                      `build` refreshes staging exactly as `run` would - but for the selected --target rather than the host - validates the staged bundle against the declared target architecture (no execution), materializes that framework train's `phoxal-supervisor`, and archives the release deterministically. A release carries its supervisor, so installing one switches the supervisor and bundle together. The default output is a sibling of the staged release, <project>/.phoxal/<triple>.build.phoxal, and the path plus its sha256 are printed at the end.\n\n\
                      `--builder` selects where compilation happens, never a different output: `local` (the default) compiles on this host with `cargo build --target`; `container` compiles natively inside the pinned official rust image for the target platform; `ssh://user@host` snapshots the source, compiles in a remote temporary directory, and pulls back the same archive. Every backend produces the identical deterministic build.phoxal."
    )]
    Build(build::Build),
    #[command(
        about = "Build and install a deployment release on a prepared robot over SSH.",
        long_about = "Build a deployment release and install it on a prepared robot over SSH.\n\n\
                      A prepared robot is one carrying an executable `/usr/local/bin/phoxal` that your SSH user may run under sudo, with `phoxal-supervisor.service` already installed. That CLI is what installs the archive; deploy never provisions the device.\n\n\
                      The robot's `phoxal` need not be this client's version. A release carries the framework supervisor that executes it, so the two sides only have to agree on the release layout - and a robot CLI too old to read that layout refuses the archive by name, which is the one case worth upgrading it for."
    )]
    Deploy(deploy::Deploy),
    #[command(about = "Install one deployment release archive atomically.")]
    Install(install::Install),
    #[command(about = "Activate an older installed deployment release.")]
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
    #[command(
        about = "Build a fresh bundle, launch the supervisor on it, and attach.",
        long_about = "Create a fresh execution and attach a terminal session to it.\n\n\
                      `run` builds and publishes a fresh bundle, launches its `phoxal-supervisor`, and attaches over the supervisor API. It never attaches to an execution that is already live: if one answers, it says so and names `attach` and `stop` instead - you asked for a fresh execution of the code you just changed.\n\n\
                      In the session, `q` detaches and leaves the robot running; Ctrl+C opens a stop confirmation, and a second Ctrl+C confirms it."
    )]
    Run(run::Run),
    #[command(
        about = "Build a fresh bundle, launch the supervisor, wait for readiness, and exit.",
        long_about = "Create a fresh execution headlessly and return once it is ready.\n\n\
                      `start` runs the same build/publish/launch path as `run` and differs only in what it does once the supervisor answers: it waits for the graph to reach readiness, prints how to attach or stop, and exits. It never mounts the terminal UI.\n\n\
                      It is not the systemd verb. The unit starts `phoxal-supervisor <BUNDLE_ROOT>` directly, and the supervisor owns `READY=1` and the watchdog - it is the only process that knows when the graph became ready."
    )]
    Start(start::Start),
    #[command(
        about = "Attach a terminal session to an execution that is already running.",
        long_about = "Attach to an existing execution. This command never builds and never mutates the project: it resolves the endpoint from the local manifest (or takes one explicitly with --endpoint), completes the supervisor handshake, and verifies that the running bundle is the robot it claims to be.\n\n\
                      `q` detaches and leaves the robot running. Ctrl+C opens a stop confirmation; a second Ctrl+C confirms it."
    )]
    Attach(attach::Attach),
    #[command(about = "End a running execution and wait for it to stop.")]
    Stop(stop::Stop),
    #[command(about = "Read the supervisor's retained participant logs.")]
    Logs(logs::Logs),
    #[command(about = "Report a running execution's authoritative snapshot.")]
    Status(status::Status),
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Manage the systemd phoxal-supervisor.service.")]
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
}
