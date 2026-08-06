//! The identity every observation in one attachment is stamped with.

use phoxal_cli_core::identity::ExecutionId;

/// Identity shared by every source and store update in one attachment.
///
/// It is exactly the execution: one `phoxald` is one router is one
/// `ExecutionId` is one execution (organization#978), so a *new execution means
/// a new attachment* - a fresh daemon has a fresh key space and nothing from
/// the previous attachment carries over. There is no supervisor generation and
/// no graph generation to compare first: the daemon cannot restart the graph
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
