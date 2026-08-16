//! Application-neutral client access to one running Phoxal execution.
//!
//! [`Connection::connect`] resolves one configured endpoint, completes the
//! frozen supervisor bootstrap, validates the current framework compatibility
//! line, and opens the current typed protocol. The unique [`Connection`] owns
//! transport lifetime and deterministic close; cloneable [`Client`] values can
//! perform typed operations but cannot create, replace, or close the session.
//!
//! Endpoint operations are derived from generated protocol descriptors:
//!
//! - State, Sample, Event, and `Stream<_, Out>` are received from owners;
//! - Setpoint and `Stream<_, In>` are published to owners;
//! - Query uses the generated request/response descriptor and the framework's
//!   current bounded timeout.
//!
//! Losing the supervisor identity, the snapshot stream, or an owner-owned bus
//! worker is terminal for the connection. [`Client::disconnect_reason`] retains
//! the first structured cause. Retry, reconnect, endpoint selection, and broader
//! timeout budgets remain decisions for the application using this crate.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod connection;
mod error;
mod router;

pub use connection::{Client, ConnectOptions, Connected, Connection};
pub use error::{ClientError, CloseError, CompatibilityRefusal, ConnectError, DisconnectReason};

/// Typed endpoint handles, descriptor bounds, and observable transport errors
/// that appear in client operations. Session construction and lifetime types
/// are intentionally absent.
pub use phoxal_bus::{
    AskQuery, BusError, BusFault, ClientPublishContract, ClientReceiveContract, EventContract,
    EventReceiver, Observed, Publish, Querier, QueryEndpointDescriptor, QueryError, QueryFailure,
    ReceiveTerminal, SampleDeliveryContract, SampleReceiver, SetpointContract, SetpointPublisher,
    SourceLabelError, StateDeliveryContract, StateView, StreamContract, StreamDeliveryContract,
    StreamEvent, StreamPublisher, StreamReceiver, Subscribe, Topic,
};

/// Framework value types exposed by connection facts and fenced commands.
pub use phoxal_runtime_contract::{
    identity::{ExecutionId, ParticipantId, ProducerId, RobotId},
    version::FrameworkVersion,
};

/// The robot-domain protocol family.
pub use phoxal_protocol::robot;

/// The process-runtime protocol family.
pub use phoxal_protocol::runtime;

/// The supervisor protocol family.
pub use phoxal_protocol::supervisor;
