//! Supervisor-owned execution lifecycle.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectLifecycle {
    Starting,
    Ready,
    Failed,
    Stopping,
    Stopped,
}
