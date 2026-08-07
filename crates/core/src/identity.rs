//! Shared framework identities used across CLI ownership boundaries.
//!
//! CLI crates import these identities through this module so the framework path
//! is confined to one explicit dependency seam.
//!
//! Nothing here is minted by the CLI any more. An `ExecutionId` is the
//! supervisor's own router session ZID, and a `ProducerId` is the ZID of the
//! session a participant opens - so both are facts about a live session rather
//! than values a supervisor decided in advance.

pub use phoxal_runtime_contract::{ExecutionId, ProducerId};
