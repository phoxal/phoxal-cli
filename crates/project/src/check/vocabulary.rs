//! Project participant classification and graph-report vocabulary.

use phoxal::model::connection::ConnectionKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantApis {
    pub participant_id: String,
    pub artifact_id: String,
    pub participant_kind: ParticipantKind,
    pub config_schema: Option<serde_json::Value>,
    /// The one connection kind this binary declared it accepts
    /// (`#[phoxal::driver(connection = ...)]`), or `None` when it declared
    /// none - which is every service and brain, and a driver that takes the
    /// whole connection and decides for itself.
    pub connection: Option<ConnectionKind>,
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

/// Which authored slot a rejected configuration was read from.
///
/// A `services.<id>.config` and a component instance's
/// `robot.components.<id>.driver.config` are both authored configuration
/// validated against an embedded schema, and a diagnostic that calls a driven
/// instance a "user runtime" names the wrong thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOwner {
    /// A declared `services.<id>` entry.
    UserService,
    /// A driven `robot.components.<id>` entry's `driver:` block.
    ComponentDriver,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    InvalidConfig {
        owner: ConfigOwner,
        runtime_id: String,
        errors: Vec<String>,
    },
    /// A driven instance authors a connection its driver binary does not
    /// accept. The binary refuses this at startup, before its bus opens, so
    /// catching it here is the difference between a red `validate` and a robot
    /// whose drivers all die at Ready.
    ConnectionKindMismatch {
        instance: String,
        artifact_id: String,
        declared: ConnectionKind,
        authored: ConnectionKind,
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
