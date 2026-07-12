//! The consumer-facing Phoxal CLI (`phoxal-cli`).
//!
//! `phoxal-cli` is the tool a robot developer runs from a robot project. It
//! reads `robot.yaml`, resolves the graph against a verified generated artifact
//! catalog ([`catalog`]) when official artifacts are needed, and drives the
//! local develop/simulate/deploy loop. Official services, drivers, tools, and
//! simulators come from the configured catalog or local development overrides.
//!
//! The command surface (see [`commands`]) is:
//!
//! - `check` - collect each participant's compiled-in contract metadata
//!   (extracted straight from its built binary's linker section, never by
//!   executing it - [`participant_metadata`]) and validate it with the shared
//!   [`phoxal::check`] graph core; git component commits resolve live unless
//!   pinned to a commit SHA in `robot.yaml`.
//! - `simulate <world>` - resolve and print or run the host-native simulation plan.
//! - `deploy <user@host>` - render the checked launch plan into a native systemd
//!   payload, sync it to the robot, restart `phoxal.target`, and report health.
//!   Prints the v0 pre-stable warning.
//! - `service catalog` - print official services from the configured artifact catalog.
//! - `update` - resolve heads and atomically update the project-vendored set.
//! - `doctor` - check host prerequisites; `self upgrade` - update the CLI.
//!
//! Official binaries are resolved into the project-local `.phoxal/` store by
//! `update`; normal commands consume that vendored selection without mutating
//! it. `validate` remains the lower-level structural/dependency predecessor of
//! `check`.

#![allow(clippy::module_name_repetitions)]

pub mod catalog;
pub mod commands;
pub(crate) mod component_driver;
pub mod context;
pub(crate) mod git_artifact;
pub mod host_doctor;
pub mod host_paths;
pub mod identity;
pub mod launch_env;
pub mod launch_plan;
pub mod native_artifacts;
pub mod native_pending;
pub mod output_mode;
pub mod participant_metadata;
pub mod process;
pub mod progress;
pub mod project;
pub mod resolver;
pub mod shell;
pub mod simulate_staging;
pub mod supervisor;
pub mod theme;
pub mod ui;
pub(crate) mod update_notice;
pub mod utils;
pub mod watch;
pub mod webots_stage_root;
pub mod webots_staging;
pub mod world;

pub use context::AppContext;
pub use project::Project;
pub use ui::Ui;
