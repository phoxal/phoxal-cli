//! The consumer-facing Phoxal CLI (`phoxal-cli`).
//!
//! `phoxal-cli` is the tool a robot developer runs from a robot project. It
//! reads `robot.yaml`, resolves the graph against its compiled-in official
//! service catalog ([`catalog`]), and drives the local develop/simulate/deploy
//! loop. The CLI ships with no framework workspace: its authority over the
//! official service set is the compiled-in catalog, with native distribution
//! metadata landing in the follow-up work.
//!
//! The command surface (see [`commands`]) is:
//!
//! - `check` - collect each participant's `emit-apis` metadata, then validate
//!   per-contract wire-shape (`schema_id`) agreement and topology with the shared
//!   [`phoxal::check`] graph core; git component commits resolve live unless
//!   pinned to a commit SHA in `robot.yaml`.
//! - `simulate <world>` - resolve and stage the run bundle. Live native launch
//!   lands in the native distribution work.
//! - `deploy build` - reserved for the native systemd deployment bundle.
//! - `robot new <name>` - scaffold a robot project.
//! - `service add|run|catalog` - manage user service crates.
//! - `pull` / `outdated` - reserved for native release asset distribution.
//! - `doctor` - check host prerequisites; `self upgrade` - update the CLI.
//!
//! There is no lockfile: tool versions and component commits resolve live from
//! GitHub releases and `git ls-remote` when a command needs them. `validate`
//! remains as the lower-level structural/dependency predecessor of `check`.

#![allow(clippy::module_name_repetitions)]

pub mod catalog;
pub mod commands;
pub(crate) mod component_driver;
pub mod context;
pub mod host_doctor;
pub mod host_paths;
pub mod native_pending;
pub mod process;
pub mod project;
pub mod resolver;
pub mod run_view;
pub mod shell;
pub mod simulator_staging;
pub mod tool_provisioning;
pub mod ui;
pub mod utils;
pub mod webots_staging;
pub mod world;

pub use context::AppContext;
pub use project::Project;
pub use ui::Ui;
