//! Projection from daemon-owned process state to the public supervisor contract.

use std::time::SystemTime;

use crate::model as core;
use phoxal_api::supervisor::snapshot::{
    DaemonFailure, DesiredState, ExitStatus, Lifecycle, Process, ProcessFailure,
    ProcessFailureKind, ProcessState, Snapshot, StartupStep, WallTime,
};

use super::roster::Roster;

#[derive(Clone, Debug)]
pub(crate) struct ExecutionFacts {
    pub(crate) roster: Roster,
}

pub(crate) fn project(
    revision: u64,
    facts: &ExecutionFacts,
    startup: &[StartupStep],
    failure: Option<&DaemonFailure>,
    board: &crate::state::board::Board,
) -> Snapshot {
    let mut processes: Vec<_> = facts
        .roster
        .entries()
        .map(|entry| {
            let status = board
                .processes
                .get(&entry.key)
                .map(|process| &process.status);
            Process {
                participant: entry.key.participant().clone(),
                kind: entry.kind,
                component: entry.component.clone(),
                desired: DesiredState::Running,
                state: status.map_or(ProcessState::Starting, |status| state(status.actual)),
                pid: status.and_then(|status| status.pid),
                producer: status.and_then(|status| status.producer),
                restarts: status.map_or(0, |status| status.restart_count_total),
                failure: status
                    .and_then(|status| status.last_failure.as_ref())
                    .map(process_failure),
            }
        })
        .collect();
    processes.sort_by(|left, right| left.participant.cmp(&right.participant));
    Snapshot {
        revision,
        lifecycle: lifecycle(board.lifecycle),
        startup: startup.to_vec(),
        processes,
        failure: failure.cloned(),
    }
}

const fn lifecycle(value: core::ProjectLifecycle) -> Lifecycle {
    match value {
        core::ProjectLifecycle::Starting => Lifecycle::Starting,
        core::ProjectLifecycle::Ready => Lifecycle::Ready,
        core::ProjectLifecycle::Degraded => Lifecycle::Degraded,
        core::ProjectLifecycle::Failed => Lifecycle::Failed,
        core::ProjectLifecycle::Stopping => Lifecycle::Stopping,
        core::ProjectLifecycle::Stopped => Lifecycle::Stopped,
    }
}

const fn state(value: core::ProcessState) -> ProcessState {
    match value {
        core::ProcessState::Starting => ProcessState::Starting,
        core::ProcessState::Ready => ProcessState::Ready,
        core::ProcessState::Degraded => ProcessState::Degraded,
        core::ProcessState::Failed => ProcessState::Failed,
        core::ProcessState::Restarting => ProcessState::Restarting,
        core::ProcessState::Stopped => ProcessState::Stopped,
    }
}

const fn failure_kind(value: core::ProcessFailureKind) -> ProcessFailureKind {
    match value {
        core::ProcessFailureKind::Spawn => ProcessFailureKind::Spawn,
        core::ProcessFailureKind::Exit => ProcessFailureKind::Exit,
        core::ProcessFailureKind::ReadinessTimeout => ProcessFailureKind::ReadinessTimeout,
        core::ProcessFailureKind::ReadinessConflict => ProcessFailureKind::ReadinessConflict,
        core::ProcessFailureKind::Cleanup => ProcessFailureKind::Cleanup,
    }
}

fn process_failure(value: &core::ProcessFailure) -> ProcessFailure {
    let exit = value.exit.as_ref().and_then(|exit| {
        exit.code
            .map(|code| ExitStatus::Code { code })
            .or_else(|| exit.signal.map(|signal| ExitStatus::Signal { signal }))
    });
    ProcessFailure::new(
        failure_kind(value.kind),
        wall_time(value.occurred_at),
        exit,
        value.detail.as_str(),
        value.stderr_tail.as_ref().map(|tail| tail.as_str()),
    )
}

fn wall_time(value: SystemTime) -> WallTime {
    WallTime::from_system_time(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_bus::{Codec, MessagePack};

    #[test]
    fn projection_orders_an_unsorted_persisted_roster_for_the_wire_contract() {
        let facts = ExecutionFacts {
            roster: Roster::out_of_order_test_fixture(),
        };
        let snapshot = project(1, &facts, &[], None, &crate::state::board::Board::default());
        snapshot.validate().expect("projected snapshot validates");
        MessagePack::encode(&snapshot).expect("projected snapshot encodes");
        assert_eq!(snapshot.processes[0].participant.as_str(), "brain");
        assert_eq!(snapshot.processes[1].participant.as_str(), "webots");
    }
}
