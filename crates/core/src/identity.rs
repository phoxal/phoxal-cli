//! Shared framework identities used across CLI ownership boundaries.
//!
//! CLI crates import these identities through this module so the framework
//! path is confined to one explicit dependency seam.
//!
//! The supervisor pre-mints each spawned participant's [`ProducerId`]. A
//! restart is structurally a different producer, so supervisor restart fencing
//! and bus-level producer fencing share that identity instead of maintaining a
//! parallel incarnation counter.

pub use phoxal::bus::{ExecutionId, ProducerId};
