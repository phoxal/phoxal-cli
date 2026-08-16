//! Project participant classification and graph-report vocabulary.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantApis {
    pub participant_id: String,
    pub artifact_id: String,
    pub participant_kind: ParticipantKind,
    pub config_schema: Option<serde_json::Value>,
    pub scope: ParticipantScope,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParticipantKind {
    /// The one mandatory root brain. A real variant, never
    /// the lenient [`Self::Other`] fallback: the brain is a first-class kind
    /// this CLI release understands end to end.
    Brain,
    Service,
    Driver,
    /// A kind this CLI release does not know. It is kept rather than rejected
    /// so a binary from a newer line still reports what it claims to be.
    Other(String),
}

impl ParticipantKind {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "brain" => Self::Brain,
            "service" => Self::Service,
            "driver" => Self::Driver,
            other => Self::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ParticipantScope {
    #[default]
    Graph,
    ComponentInstance(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    InvalidConfig {
        runtime_id: String,
        errors: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub problems: Vec<Problem>,
}

impl Report {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}
