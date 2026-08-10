//! Supervisor lifecycle, readiness, and failure-policy values.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupRequirement {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailurePolicy {
    KeepProjectDegraded,
    StopProject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLifecycle {
    Starting,
    Ready,
    Degraded,
    Failed,
    Stopping,
    Stopped,
}
