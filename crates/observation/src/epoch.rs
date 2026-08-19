//! The identity every observation in one attachment is stamped with.

use phoxal::identity::ExecutionId;

/// Identity shared by every source and store update in one attachment.
///
/// It is exactly the execution: one framework supervisor is one router is one
/// `ExecutionId` is one execution, so a *new execution means
/// a new attachment* - a fresh supervisor has a fresh key space and nothing from
/// the previous attachment carries over. There is no supervisor generation and
/// no graph generation to compare first: the supervisor cannot restart the graph
/// under a client without also having minted a new execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttachmentEpoch {
    pub execution: ExecutionId,
}

impl AttachmentEpoch {
    #[must_use]
    pub const fn new(execution: ExecutionId) -> Self {
        Self { execution }
    }
}
