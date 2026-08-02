//! Child lifecycle, startup stages, restart policy, and orderly shutdown.

pub(crate) mod child;
pub(crate) mod output;
pub(crate) mod participant;
pub(crate) mod policy;
pub(crate) mod signals;
pub(crate) mod spec;
pub(crate) mod stages;
pub(crate) mod supervise;

pub(crate) use crate::{
    MAX_CAPTURED_LINE_BYTES, ParticipantSpec, ProcessState, SupervisionStage, SupervisorAction,
    SupervisorOptions, SupervisorState,
};
pub(crate) use output::{join_reader, requested_stop_exit_is_clean, spawn_output_reader};
pub(crate) use participant::RunningParticipant;
pub(crate) use policy::RestartPolicy;
pub(crate) use signals::{
    ensure_process_group_stopped, kill_child_process_group, send_process_group_signal,
    send_process_group_terminate, send_process_signal, send_terminate, stop_child,
};
pub(crate) use spec::{RequestedStop, SupervisorActionReceiver};
pub(crate) use stages::{await_stage_ready, maybe_publish_startup_outcome, spawn_until_pending};
