use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Connecting,
    Live,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceHealth {
    pub sources: BTreeMap<String, SourceStatus>,
}
