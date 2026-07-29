//! Command and private-bootstrap wire DTOs.

use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_core::runtime::ProcessKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CommandSessionId(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandKey {
    pub session: CommandSessionId,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandAction {
    Restart {
        process: ProcessKey,
        expected_producer: ProducerId,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    pub supervisor_generation: u64,
    pub key: CommandKey,
    pub action: CommandAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReply {
    pub accepted: bool,
    pub error: Option<CommandError>,
}

impl CommandReply {
    #[must_use]
    pub const fn accepted() -> Self {
        Self {
            accepted: true,
            error: None,
        }
    }

    #[must_use]
    pub const fn rejected(error: CommandError) -> Self {
        Self {
            accepted: false,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandError {
    StaleSupervisorGeneration,
    InvalidSession,
    AlreadyProcessed,
    OutOfOrder,
    UnknownProcess,
    SupersededProducer,
    SupervisorUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum BootstrapResult {
    Bound {
        supervisor_generation: u64,
        /// The execution identity minted by the launcher and adopted by the
        /// resident through their private socketpair.
        execution: ExecutionId,
    },
    Rejected {
        error: String,
    },
}
