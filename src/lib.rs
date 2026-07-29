//! The consumer-facing Phoxal CLI (`phoxal-cli`).
//!
//! `phoxal-cli` is the tool a robot developer runs from a robot project. It
//! reads `robot.yaml`, resolves the graph against the CLI-internal official
//! catalog ([`phoxal_cli_core::project::catalog`]) and the project's exact
//! locked framework train, and drives the local develop/simulate loop.
//! Official services, drivers, tools, and simulators materialize via `cargo
//! install <package>@<train> --registry phoxal --locked` straight into the
//! staged runtime layout's flat `bin/` store (organization#951 WS4) - there
//! is no separate download/vendoring step and no project-local artifact
//! store; Cargo's own registry cache is the only cache involved.
//!
//! The command surface (see [`commands`]) is:
//!
//! - `build`/`run`/`simulation webots run <world>` - resolve the graph, stage
//!   it (materializing every official and building every source/override
//!   participant into an unpublished candidate, publishing only once staging
//!   and the loader's own validation both succeed), then collect each
//!   participant's compiled-in contract metadata (extracted straight from
//!   its built binary's linker section, never by executing it -
//!   `participant_metadata`) and validate it with the shared
//!   [`phoxal::check`] graph core before launching; Cargo.lock resolves all
//!   project source.
//! - `service suite` - print official services from the catalog at the
//!   project's locked train.
//! - `doctor` - check host prerequisites; `self upgrade` - update the CLI.
//!
//! `validate` remains the lower-level structural/dependency predecessor of
//! the participant identity and config validation that `build`, `run`, and
//! `simulation webots run` perform.

#![allow(clippy::module_name_repetitions)]

pub(crate) mod application;
pub(crate) mod cli;
pub mod commands;
pub(crate) mod context;
pub(crate) mod host_doctor;
pub mod run;
pub mod simulation;
pub(crate) mod ui;

pub use context::AppContext;
// Re-exported so `main`'s tracing-subscriber setup can install this as its
// `MakeWriter` (finding C2/A2): `session` itself stays `pub(crate)` - nothing
// outside this crate needs the rest of it - but `main` is a SEPARATE crate
// (the `phoxal-cli` binary), so this ONE type must stay nameable from there
// or tracing output can never be routed through an active session's
// diagnostics again (see `cli::output::diagnostics`'s own module docs).
pub use cli::output::diagnostics::SessionAwareWriter;
pub use phoxal_cli_supervisor::{active_execution, maybe_run_guardian};
pub use ui::Ui;
