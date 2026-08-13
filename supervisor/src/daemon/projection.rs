//! Projection from daemon-owned process state to the public supervisor contract.

use std::time::SystemTime;

use crate::model::lifecycle::ProjectLifecycle;
use crate::model::process::{
    ProcessFailure as CoreProcessFailure, ProcessFailureKind as CoreProcessFailureKind,
    ProcessState as CoreProcessState,
};
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

const fn lifecycle(value: ProjectLifecycle) -> Lifecycle {
    match value {
        ProjectLifecycle::Starting => Lifecycle::Starting,
        ProjectLifecycle::Ready => Lifecycle::Ready,
        ProjectLifecycle::Failed => Lifecycle::Failed,
        ProjectLifecycle::Stopping => Lifecycle::Stopping,
        ProjectLifecycle::Stopped => Lifecycle::Stopped,
    }
}

const fn state(value: CoreProcessState) -> ProcessState {
    match value {
        CoreProcessState::Starting => ProcessState::Starting,
        CoreProcessState::Ready => ProcessState::Ready,
        CoreProcessState::Failed => ProcessState::Failed,
        CoreProcessState::Restarting => ProcessState::Restarting,
    }
}

const fn failure_kind(value: CoreProcessFailureKind) -> ProcessFailureKind {
    match value {
        CoreProcessFailureKind::Spawn => ProcessFailureKind::Spawn,
        CoreProcessFailureKind::Exit => ProcessFailureKind::Exit,
        CoreProcessFailureKind::ReadinessTimeout => ProcessFailureKind::ReadinessTimeout,
        CoreProcessFailureKind::ReadinessConflict => ProcessFailureKind::ReadinessConflict,
        CoreProcessFailureKind::Cleanup => ProcessFailureKind::Cleanup,
    }
}

fn process_failure(value: &CoreProcessFailure) -> ProcessFailure {
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
