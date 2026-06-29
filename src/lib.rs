//! The consumer-facing Phoxal CLI (`phoxal-cli`).
//!
//! `phoxal-cli` is the tool a robot developer runs from a robot project. It
//! reads `robot.yaml`, resolves the graph against its compiled-in official
//! runtime catalog ([`catalog`]), and drives the local develop/simulate/deploy
//! loop. The CLI ships with no framework workspace: its authority over the
//! official runtime set is the compiled-in catalog, and compatibility is proven
//! by running each artifact's `emit-apis` rather than by trusting a descriptor.
//!
//! The command surface (see [`commands`]) is:
//!
//! - `check` - validate the resolved graph's single `api_version` and topology
//!   via `emit-apis`; offline by default, `--pull` to refresh official images.
//! - `simulate <world>` - resolve, generate the run bundle, and launch the
//!   Webots simulation stack.
//! - `deploy build` - produce an immutable, digest-pinned deployment artifact.
//! - `robot new <name>` - scaffold a robot project.
//! - `runtime add|run|image|catalog` - manage user runtime crates.
//! - `pull` / `outdated` - refresh, or report drift in, cached official images
//!   and host tools.
//! - `doctor` - check host prerequisites; `self upgrade` - update the CLI.
//!
//! `validate` and `update` are the lower-level predecessors of `check` and the
//! lockfile half of `pull`; they remain for the lock-file/report workflows that
//! still drive `phoxal.sources.lock` directly.

#![allow(clippy::module_name_repetitions)]

pub mod catalog;
pub mod check;
pub mod commands;
pub(crate) mod component_driver;
pub mod compose;
pub mod context;
pub mod docker_stack;
pub mod host_config;
pub mod host_doctor;
pub mod host_paths;
pub mod local_build;
pub mod local_zenoh;
pub mod lockfile;
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
