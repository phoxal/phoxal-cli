//! The bus vocabulary this crate's own surface speaks.
//!
//! An attachment hands out a live session ([`BusHandle`]) so a caller can open
//! the robot-domain views and publishers the supervisor contract does not
//! carry, and its failures name bus and query errors directly. Those types are
//! part of this boundary's public API, so they are re-exported here rather
//! than reached for through the framework crates.
//!
//! Nothing is wrapped: the current protocol's transport is the transport. A
//! second protocol would keep its own transport types behind this crate, and
//! whatever survives here would be the subset every protocol shares.

pub use phoxal_bus::{
    BusConfig, BusError, BusHandle, BusOwner, QueryError, QueryFailure, SetpointPublisher,
    SetpointReceiver, StateView, StreamReceiver,
};
