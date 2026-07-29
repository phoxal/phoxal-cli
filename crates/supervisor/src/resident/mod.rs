//! Project-local resident-supervisor authority and transport.

mod bootstrap;
mod command_sessions;
mod force_stop;
mod launcher;
mod log;
mod server;
mod socket;

pub use bootstrap::{private_bootstrap_execution, report_private_bootstrap};
pub use force_stop::{
    DETACHED_FORCE_STOP_GRACE, ForceStopOutcome, SYSTEMD_FORCE_STOP_GRACE, force_stop,
};
pub use launcher::{LaunchedResident, has_private_bootstrap, launch_detached};
pub use socket::{ResidentSocket, supervisor_socket_path};
