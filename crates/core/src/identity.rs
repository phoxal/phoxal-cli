//! Shared framework identities used across CLI ownership boundaries.
//!
//! CLI crates import these identities through this module so the framework
//! path is confined to one explicit dependency seam.
//!
//! The supervisor pre-mints each spawned participant's [`ProducerId`]. A
//! restart is structurally a different producer, so supervisor restart fencing
//! and bus-level producer fencing share that identity instead of maintaining a
//! parallel incarnation counter.

pub use phoxal_runtime_contract::{ExecutionId, ProducerId};

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::ProducerId;

    #[test]
    fn minted_producer_identities_are_distinct_across_a_process_batch() {
        let identities = (0..1024)
            .map(|_| ProducerId::mint())
            .collect::<HashSet<_>>();
        assert_eq!(identities.len(), 1024);
    }
}
