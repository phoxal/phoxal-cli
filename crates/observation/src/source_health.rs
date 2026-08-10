use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Connecting,
    Live,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObservationSource {
    Supervisor,
    Logs,
    Telemetry,
    Motion,
    Input,
}

impl ObservationSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Supervisor => "supervisor",
            Self::Logs => "logs",
            Self::Telemetry => "telemetry",
            Self::Motion => "motion",
            Self::Input => "input",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceHealth {
    pub sources: BTreeMap<ObservationSource, SourceStatus>,
    pub ingress_dropped: u64,
}
