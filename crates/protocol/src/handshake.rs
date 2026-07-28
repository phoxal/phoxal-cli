//! Connection-role negotiation for the resident socket.

use serde::{Deserialize, Serialize};

use crate::CommandSessionId;

pub const SUPERVISOR_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionRole {
    Snapshots,
    Commands,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeRequest {
    pub protocol_version: u16,
    pub role: ConnectionRole,
    pub resume_command_session: Option<CommandSessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeReply {
    pub protocol_version: u16,
    pub supervisor_generation: u64,
    pub command_session: Option<CommandSessionId>,
}
