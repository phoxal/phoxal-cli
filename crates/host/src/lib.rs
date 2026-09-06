//! CLI host policy layered on the framework runtime rendezvous contract.
//!
//! The contract itself is `phoxal::supervisor::rendezvous`, and a caller that
//! wants it names it. What lives here is the CLI's own layout around it.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

pub mod paths;
pub mod world;
pub mod world_process;
