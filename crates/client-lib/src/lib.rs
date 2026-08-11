//! The external client boundary: everything a client, a renderer, or an
//! observer needs in order to talk to a running Phoxal robot.
//!
//! # The contract this crate owns
//!
//! Code that observes or drives a remote robot depends on `phoxal_client` and
//! on nothing else remote. The framework's wire crates (`phoxal-api`,
//! `phoxal-bus`) are this crate's dependencies, not its consumers'. That
//! dependency direction is the whole point: when the robot's protocol changes
//! shape, exactly one crate in this workspace has to learn the new shape.
//!
//! # What it is today
//!
//! A facade over the current protocol's vocabulary, not a second model of it.
//! [`robot`], [`runtime`], and [`supervisor`] re-export the framework's typed
//! families unchanged, and [`transport`] re-exports the bus types that appear
//! in this crate's own signatures. Wrapping a wire type in a client-owned twin
//! would add a translation layer that says nothing the wire type does not
//! already say, so it is deliberately not done.
//!
//! # The protocol-selection seam
//!
//! [`Attachment::open`] already runs the sequence a multi-protocol client
//! needs: bootstrap over the frozen `supervisor/connect` endpoint, read the
//! remote's exact [`FrameworkVersion`], then decide which implementation
//! speaks to it. Today that decision has one outcome - the current protocol,
//! or a refusal naming both sides - because one protocol exists. Historical
//! adapters and per-protocol capability queries are absent for the same
//! reason, and adding them means filling in the branch that is already there
//! rather than reshaping the flow around it.
//!
//! When a second protocol does arrive, the rule this crate is built to keep is
//! that a historical protocol's wire types never appear in its public API. A
//! caller asks for a snapshot, a command outcome, or a log page; which
//! protocol produced it is this crate's business alone.
//!
//! # Attachment ownership
//!
//! [`Attachment`] uniquely owns the transport and its background work.
//! Cloneable [`AttachmentPort`] values borrow that attachment's authority
//! without gaining the ability to close it. Reattaching always performs a
//! fresh handshake; an execution-id change is a new run, never a resumed
//! session.
//!
//! [`FrameworkVersion`]: phoxal_runtime_contract::version::FrameworkVersion

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod attachment;
mod error;
mod router;
pub mod transport;

pub use attachment::{Attachment, AttachmentConfig, AttachmentPort, Connected};
pub use error::AttachError;

/// The robot domain a participant authors against: motion state, the manual
/// command an operator publishes, and the topics that carry them.
pub use phoxal_api::robot;

/// What a running Phoxal process reports about itself: log events, bus and
/// step telemetry, and the authoritative simulation clock.
pub use phoxal_api::runtime;

/// The supervisor vocabulary: the connect bootstrap, snapshots, commands, the
/// immutable run info, and the bounded log and telemetry histories.
pub use phoxal_api::supervisor;
