//! Project loading, validation, build, materialization, and staging ownership.
//!
//! This crate turns authored project inputs into validated runtime plans and
//! resolve logical roots into shared runtime targets. It must not parse CLI
//! arguments, render terminal output, or own supervisor/client lifecycle.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod build;
mod bundle;
mod check;
mod deployment;
pub mod host;
mod load;
mod paths;
mod progress;
mod progress_phase;
pub mod registry;
mod registry_package;
mod resolve;
mod run;
mod runtime_target;
mod schema;
mod simulation;
pub mod source;
mod stage;
mod target;
mod validation;

pub use build::container::ContainerEngine;
pub use build::shell::shell_quote;
pub use build::{BuildBackend, BuildBundleRequest, BuiltBundle, build_bundle};
pub use deployment::{
    BUNDLE_DIR, EXECUTOR_FILE, ExecutorSource, ReleaseLayout, SharedExecutorSource,
};
pub use paths::runtime::{
    ACTIVE_RUNTIME_ROOT, INSTALL_ROOT, INSTALLED_BINARY_ROOT, INSTALLED_CLIENT_BINARY,
    INSTALLED_DAEMON_BINARY, INSTALLED_STATE_ROOT, INSTALLED_VOLATILE_ROOT, RELEASES_ROOT,
    RuntimePaths, SYSTEMD_ACTIVE_ROOT, SYSTEMD_UNIT, SYSTEMD_UNIT_PATH, SYSTEMD_UNIT_ROOT,
};
pub use progress::{PreparationEvent, Reporter, SilentReporter};
pub use progress_phase::{PhaseId, PhaseOutcome};
pub use run::{DriverMode, DriverRequest, PrepareRunRequest, PreparedExecution, prepare_run};
pub use runtime_target::{RuntimeAuthority, RuntimeTarget};
pub use simulation::{
    PrepareSimulationRequest, PreparedSimulation, StageWebotsRequest, WebotsHost, WebotsLaunch,
    prepare_simulation, stage_webots,
};
pub use target::resolve as resolve_target;
pub use validation::{
    ArchiveValidation, ValidateRequest, ValidationComponent, ValidationReport, ValidationSource,
    validate,
};
