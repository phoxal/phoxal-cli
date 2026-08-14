//! CLI host policy layered on the framework runtime rendezvous contract.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

pub mod advisory;
pub mod paths;
