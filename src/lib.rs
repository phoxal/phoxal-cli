//! The consumer-facing Phoxal CLI (`phoxal-cli`).
//!
//! `phoxal-cli` is the tool a robot developer runs from a robot project. It
//! reads `robot.yaml`, resolves the graph against a verified generated artifact
//! catalog ([`catalog`]) when official artifacts are needed, and drives the
//! local develop/simulate/deploy loop. The only compiled-in catalog remnant is
//! the small host-tool version table; official service and driver names come
//! from the configured catalog or the robot workspace.
//!
//! The command surface (see [`commands`]) is:
//!
//! - `check` - collect each participant's `emit-apis` metadata, then validate
//!   per-contract wire-shape (`schema_id`) agreement and topology with the shared
//!   [`phoxal::check`] graph core; git component commits resolve live unless
//!   pinned to a commit SHA in `robot.yaml`.
//! - `simulate <world>` - resolve and print the host-native launch plan. Live
//!   native launch lands in the native distribution work.
//! - `deploy build` - reserved for the native systemd deployment release artifact.
//! - `robot new <name>` - scaffold a robot project.
//! - `service add|catalog` - manage user service crates.
//! - `generations status` - inspect catalog readiness for the robot target.
//! - `pull` / `outdated` - refresh or inspect catalog and native asset state.
//! - `doctor` - check host prerequisites; `self upgrade` - update the CLI.
//!
//! There is no lockfile: catalog revisions, tool versions, and component commits
//! resolve live or from the local cache when a command needs them. `validate`
//! remains as the lower-level structural/dependency predecessor of `check`.

#![allow(clippy::module_name_repetitions)]

pub mod catalog;
pub mod commands;
pub(crate) mod component_driver;
pub mod context;
pub mod host_doctor;
pub mod host_paths;
pub mod launch_env;
pub mod launch_plan;
pub mod native_pending;
pub mod process;
pub mod project;
pub mod resolver;
pub mod shell;
pub mod simulator_staging;
pub mod supervisor;
pub mod tool_provisioning;
pub mod ui;
pub mod utils;
pub mod watch;
pub mod webots_staging;
pub mod world;

pub use context::AppContext;
pub use project::Project;
pub use ui::Ui;
