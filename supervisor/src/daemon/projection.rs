//! The one place internal supervisor state becomes a wire snapshot.
//!
//! The store ([`crate::SupervisorState`]) stays typed on its own internal
//! model: it is the authority the process machinery writes to on every spawn,
//! exit, and liveliness token, and typing it on a wire DTO would make every
//! such write a protocol edit. This module is the boundary - a pure function
//! from "what the daemon knows" to "what an attached client is told".
//!
//! Everything the wire snapshot carries that the store has no concept of - the
//! robot's stable identity, the execution mode, the daemon's own startup
//! sequence, and the typed daemon failure - is supplied alongside it here
//! rather than smuggled into the store.

use std::time::SystemTime;

use phoxal_cli_core::runtime as core;
use phoxal_supervisor_api::{
    DaemonFailure, DesiredState, Detail, ExecutionMode, Lifecycle, Process, ProcessFailure,
    ProcessFailureKind, ProcessState, RobotIdentity, Snapshot, StartupStep, StderrTail, WallTime,
};

use super::roster::Roster;

/// The facts about one execution that never change once it has started.
#[derive(Clone, Debug)]
pub(crate) struct ExecutionFacts {
    pub(crate) robot: RobotIdentity,
    /// Straight passthrough of the finalized manifest's `clock:` field. The
    /// daemon has no simulation branch; this is reported, never acted on.
    pub(crate) mode: ExecutionMode,
    pub(crate) roster: Roster,
}

/// Project the daemon's current state into one complete wire snapshot.
///
/// The process list comes from the **roster**, not from the store's map: the
/// selected process set is decided once, by requirement derivation, so a
/// snapshot always reports exactly that set - a row for a process the store has
/// not touched yet reports `Starting`, and nothing the store happens to hold
/// under some other key can appear.
pub(crate) fn project(
    revision: u64,
    facts: &ExecutionFacts,
    startup: &[StartupStep],
    failure: Option<&DaemonFailure>,
    board: &crate::state::Board,
) -> Snapshot {
    let processes = facts
        .roster
        .entries()
        .map(|entry| {
            let status = board
                .processes
                .get(&entry.core)
                .map(|process| &process.status);
            Process {
                key: entry.wire.clone(),
                component: entry.component.clone(),
                startup: entry.startup,
                desired: status.map_or(DesiredState::Running, |status| desired(status.desired)),
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
    Snapshot {
        revision,
        robot: facts.robot.clone(),
        mode: facts.mode,
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
        core::ProcessState::Restarting => ProcessState::Restarting,
        core::ProcessState::Failed => ProcessState::Failed,
        core::ProcessState::Stopped => ProcessState::Stopped,
    }
}

const fn desired(value: core::DesiredProcessState) -> DesiredState {
    match value {
        core::DesiredProcessState::Running => DesiredState::Running,
        core::DesiredProcessState::Stopped => DesiredState::Stopped,
    }
}

const fn failure_kind(value: core::ProcessFailureKind) -> ProcessFailureKind {
    match value {
        core::ProcessFailureKind::Spawn => ProcessFailureKind::Spawn,
        core::ProcessFailureKind::Exit => ProcessFailureKind::Exit,
        core::ProcessFailureKind::ReadinessTimeout => ProcessFailureKind::ReadinessTimeout,
        core::ProcessFailureKind::Cleanup => ProcessFailureKind::Cleanup,
        core::ProcessFailureKind::Other => ProcessFailureKind::Other,
    }
}

fn process_failure(value: &core::ProcessFailure) -> ProcessFailure {
    ProcessFailure {
        kind: failure_kind(value.kind),
        occurred_at: wall_time(value.occurred_at),
        exit: value
            .exit
            .as_ref()
            .map(|exit| phoxal_supervisor_api::ExitStatus {
                code: exit.code,
                signal: exit.signal,
            }),
        detail: Detail::new(value.detail.as_str()),
        stderr_tail: value
            .stderr_tail
            .as_ref()
            .map(|tail| StderrTail::new(tail.as_str())),
    }
}

/// A wall-clock instant a client on another machine renders. A time before the
/// epoch is carried as a negative second count rather than clamped, so a host
/// with a badly wrong clock reports what it actually observed.
fn wall_time(value: SystemTime) -> WallTime {
    match value.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => WallTime {
            unix_seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
            nanos: since.subsec_nanos(),
        },
        Err(before) => {
            let before = before.duration();
            WallTime {
                unix_seconds: i64::try_from(before.as_secs())
                    .map_or(i64::MIN, |seconds| -seconds - 1),
                nanos: 1_000_000_000 - before.subsec_nanos().max(1),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::identity::ProducerId;
    use phoxal_cli_core::runtime::{
        BoundedString, ParticipantKind, ProcessKey as CoreProcessKey, RobotKey, StartupRequirement,
    };
    use phoxal_supervisor_api::{Name, ProcessKey as WireProcessKey};

    use super::*;
    use crate::SupervisorState;
    use crate::daemon::roster::tests::roster;

    fn facts() -> ExecutionFacts {
        ExecutionFacts {
            robot: RobotIdentity {
                id: Name::new("rover"),
                namespace: Name::new("demo"),
            },
            mode: ExecutionMode::Simulated,
            roster: roster(),
        }
    }

    fn board_with_every_process() -> SupervisorState {
        let board = SupervisorState::new();
        for entry in facts().roster.entries() {
            board.upsert_process(
                entry.core.clone(),
                ParticipantKind::Service,
                core::ProcessState::Starting,
                StartupRequirement::Required,
            );
        }
        board
    }

    #[test]
    fn the_projected_process_set_is_the_roster_not_whatever_the_store_holds() {
        let facts = facts();
        let board = board_with_every_process();
        // A key the store holds but the requirement set never selected must not
        // reach the wire: the snapshot reports the execution's actual set.
        board.upsert_process(
            CoreProcessKey::robot(RobotKey::new("demo", "rover"), "stowaway"),
            ParticipantKind::Service,
            core::ProcessState::Ready,
            StartupRequirement::Required,
        );

        let snapshot = project(7, &facts, &[], None, &board.snapshot());
        assert_eq!(snapshot.revision, 7);
        assert_eq!(snapshot.mode, ExecutionMode::Simulated);
        assert_eq!(
            snapshot
                .processes
                .iter()
                .map(|process| process.key.to_string())
                .collect::<Vec<_>>(),
            ["brain", "service:drive", "driver:left", "simulator:webots"]
        );
    }

    #[test]
    fn a_process_the_store_has_not_touched_reports_starting_with_no_producer() {
        let facts = facts();
        let snapshot = project(
            1,
            &facts,
            &[],
            None,
            &SupervisorState::new().snapshot(),
        );
        for process in &snapshot.processes {
            assert_eq!(process.state, ProcessState::Starting, "{}", process.key);
            assert_eq!(process.producer, None, "{}", process.key);
            assert_eq!(process.pid, None, "{}", process.key);
            assert_eq!(process.restarts, 0, "{}", process.key);
        }
    }

    #[test]
    fn producer_restarts_and_failure_evidence_cross_the_boundary_intact() {
        let facts = facts();
        let board = board_with_every_process();
        let brain = facts
            .roster
            .resolve(&WireProcessKey::Brain)
            .expect("the brain is selected")
            .core
            .clone();
        let producer = ProducerId::try_from(0x2b).expect("a producer id");
        board.set_producer(&brain, producer);
        board.set_restart_count(&brain, 1);
        board.set_restart_count(&brain, 2);
        board.set_pid(&brain, Some(4242));
        board.record_captured_stderr(&brain, "panicked at 'boom'");
        board.record_failure(
            &brain,
            core::ProcessFailureKind::Exit,
            Some(core::ExitDescription {
                code: Some(101),
                signal: None,
            }),
            "process exited with status 101",
        );

        let snapshot = project(2, &facts, &[], None, &board.snapshot());
        let row = snapshot
            .processes
            .iter()
            .find(|process| process.key == WireProcessKey::Brain)
            .expect("the brain row");
        assert_eq!(row.producer, Some(producer));
        assert_eq!(row.restarts, 2, "the total, not the per-generation count");
        assert_eq!(row.state, ProcessState::Failed);
        let failure = row.failure.as_ref().expect("the failure crosses over");
        assert_eq!(failure.kind, ProcessFailureKind::Exit);
        assert_eq!(
            failure.exit,
            Some(phoxal_supervisor_api::ExitStatus {
                code: Some(101),
                signal: None
            })
        );
        assert_eq!(failure.detail.as_str(), "process exited with status 101");
        assert!(
            failure
                .stderr_tail
                .as_ref()
                .is_some_and(|tail| tail.as_str().contains("boom"))
        );
    }

    #[test]
    fn an_oversized_detail_is_bounded_rather_than_rejected_on_the_way_out() {
        let value = core::ProcessFailure {
            kind: core::ProcessFailureKind::Other,
            occurred_at: SystemTime::UNIX_EPOCH,
            exit: None,
            detail: BoundedString::with_max_bytes("x".repeat(100_000), 100_000),
            stderr_tail: Some(BoundedString::with_max_bytes("y".repeat(100_000), 100_000)),
        };
        let projected = process_failure(&value);
        assert_eq!(projected.kind, ProcessFailureKind::Other);
        assert!(projected.detail.as_str().len() <= Detail::MAX_BYTES);
        assert!(projected.stderr_tail.expect("tail").as_str().len() <= StderrTail::MAX_BYTES);
    }

    #[test]
    fn every_lifecycle_and_process_state_has_an_exact_counterpart() {
        for (internal, wire) in [
            (core::ProjectLifecycle::Starting, Lifecycle::Starting),
            (core::ProjectLifecycle::Ready, Lifecycle::Ready),
            (core::ProjectLifecycle::Degraded, Lifecycle::Degraded),
            (core::ProjectLifecycle::Failed, Lifecycle::Failed),
            (core::ProjectLifecycle::Stopping, Lifecycle::Stopping),
            (core::ProjectLifecycle::Stopped, Lifecycle::Stopped),
        ] {
            assert_eq!(lifecycle(internal), wire);
        }
        for (internal, wire) in [
            (core::ProcessState::Starting, ProcessState::Starting),
            (core::ProcessState::Ready, ProcessState::Ready),
            (core::ProcessState::Degraded, ProcessState::Degraded),
            (core::ProcessState::Restarting, ProcessState::Restarting),
            (core::ProcessState::Failed, ProcessState::Failed),
            (core::ProcessState::Stopped, ProcessState::Stopped),
        ] {
            assert_eq!(state(internal), wire);
        }
    }
}
