//! The consumer-facing Phoxal CLI (`phoxal-cli`).
//!
//! `phoxal-cli` is the tool a robot developer runs from a robot project. It
//! reads `robot.yaml`, resolves the graph against a verified generated artifact
//! suite ([`suite`]) when official artifacts are needed, and drives the
//! local develop/simulate loop. Official services, drivers, tools, and
//! simulators come from the configured suite or local development overrides.
//!
//! The command surface (see [`commands`]) is:
//!
//! - `check` - collect each participant's compiled-in contract metadata
//!   (extracted straight from its built binary's linker section, never by
//!   executing it - [`participant_metadata`]) and validate it with the shared
//!   [`phoxal::check`] graph core; Cargo.lock resolves all project source.
//! - `simulation run <world>` - resolve and print or run the host-native simulation plan.
//! - `service suite` - print official services from the configured artifact suite.
//! - `update` - resolve heads and atomically update the project-vendored set.
//! - `doctor` - check host prerequisites; `self upgrade` - update the CLI.
//!
//! Official binaries are resolved into the project-local `.phoxal/` store by
//! `update`; normal commands consume that vendored selection without mutating
//! it. `validate` remains the lower-level structural/dependency predecessor of
//! `check`.

#![allow(clippy::module_name_repetitions)]

pub mod archive;
pub mod check;
pub mod commands;
pub(crate) mod component_driver;
pub(crate) mod context;
pub(crate) mod host_doctor;
pub mod host_paths;
pub mod loader;
pub(crate) mod native_artifacts;
pub(crate) mod progress;
pub(crate) mod resident;
pub mod resolver;
pub mod run;
pub mod runtime_header;
pub mod runtime_paths;
pub(crate) mod sd_notify;
pub(crate) mod session;
pub(crate) mod shell;
pub(crate) mod simulate_staging;
pub mod simulation;
pub(crate) mod stager;
pub(crate) mod supervisor;
pub(crate) mod telemetry;
pub(crate) mod ui;
pub(crate) mod watch;
pub(crate) mod webots_stage_root;
pub(crate) mod webots_staging;

pub use context::AppContext;
// Re-exported so `main`'s tracing-subscriber setup can install this as its
// `MakeWriter` (finding C2/A2): `session` itself stays `pub(crate)` - nothing
// outside this crate needs the rest of it - but `main` is a SEPARATE crate
// (the `phoxal-cli` binary), so this ONE type must stay nameable from there
// or tracing output can never be routed through an active session's
// diagnostics again (see `session::diagnostics`'s own module docs).
pub use session::diagnostics::SessionAwareWriter;
pub use supervisor::maybe_run_guardian;
pub use ui::Ui;
