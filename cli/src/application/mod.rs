//! The command use cases the CLI surface dispatches to.
//!
//! Each module owns one domain, and no module owns two: the execution
//! lifecycle, the terminal session, the simulation session, project builds,
//! host installation, remote deployment, the systemd unit, host checks, and
//! schema generation.

pub(crate) mod build;
pub(crate) mod deployment;
pub(crate) mod doctor;
pub(crate) mod installation;
pub(crate) mod launcher;
pub(crate) mod lifecycle;
pub(crate) mod schema;
pub(crate) mod service;
pub(crate) mod session;
pub(crate) mod simulation;
pub(crate) mod startup;
pub(crate) mod summary;
pub(crate) mod supervisor;
pub(crate) mod units;
pub(crate) mod webots;
