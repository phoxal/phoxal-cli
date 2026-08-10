//! Shared host filesystem and process-lock contract for the CLI pair.

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
