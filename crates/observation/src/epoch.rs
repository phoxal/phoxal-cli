use phoxal_cli_core::identity::ExecutionId;

/// Identity shared by every graph source and store update in one attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttachmentEpoch {
    pub supervisor_generation: u64,
    pub execution_id: ExecutionId,
    pub graph_generation: u64,
}
