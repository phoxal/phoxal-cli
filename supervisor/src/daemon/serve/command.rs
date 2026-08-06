//! Commands: restart one process, or stop the execution.
//!
//! The whole surface is two commands and one fence. There is no command
//! session, sequence, watermark, or resume: a query is already one request with
//! one answer, and the supervisor-generation the old socket protocol needed to
//! scope those is gone with it (organization#978).
//!
//! # The fence
//!
//! `Restart` carries the producer the client last saw on that process's
//! snapshot row. The supervisor compares it against the producer it learned
//! from that incarnation's own liveliness token - never against a value it
//! minted, because nothing mints one any more. Two things follow for free:
//!
//! - a restart aimed at an incarnation that has already been replaced is
//!   rejected, because the learned producer has moved on;
//! - a restart aimed at a freshly spawned incarnation that has not opened its
//!   session yet is *also* rejected, because it has no producer at all and no
//!   expectation can match `None`.
//!
//! The second is the one worth stating: "no session yet" is a fact the snapshot
//! reports, and a command fenced on a fact the supervisor does not have is a
//! command it cannot safely apply.

use crate::state::Board;
use phoxal_bus::{Bus, ServerQueryable};
use phoxal_cli_core::runtime::{ProcessKey as CoreProcessKey, ProjectLifecycle};
use phoxal_supervisor_api::{Command, CommandOutcome, CommandRejection, supervisor};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::SupervisorAction;
use crate::daemon::roster::Roster;
use crate::daemon::state::ExecutionState;

/// What the command server is allowed to do to the running execution.
#[derive(Clone, Debug)]
pub(crate) struct Control {
    /// The supervision loop's action queue - the same one the loop already
    /// serves, so a remote restart and a local one take one path.
    pub(crate) actions: mpsc::Sender<SupervisorAction>,
    /// Cancelling this ends the execution, exactly as a signal does.
    pub(crate) stop: CancellationToken,
}

/// What one command resolves to, before anything is done about it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Decision {
    Restart(CoreProcessKey),
    Stop,
    Rejected(CommandRejection),
}

/// Decide one command against the current state. Pure: it performs nothing.
pub(crate) fn decide(command: &Command, roster: &Roster, board: &Board) -> Decision {
    // An execution that is on its way out accepts nothing, including a second
    // stop: a client is told the run is already ending rather than being given
    // an acknowledgement that changes nothing.
    if matches!(
        board.lifecycle,
        ProjectLifecycle::Stopping | ProjectLifecycle::Stopped | ProjectLifecycle::Failed
    ) {
        return Decision::Rejected(CommandRejection::NotRunning);
    }
    match command {
        Command::Stop => Decision::Stop,
        Command::Restart {
            process,
            expected_producer,
        } => {
            let Some(entry) = roster.resolve(process) else {
                return Decision::Rejected(CommandRejection::UnknownProcess);
            };
            let learned = board
                .processes
                .get(&entry.core)
                .and_then(|process| process.status.producer);
            if learned != Some(*expected_producer) {
                return Decision::Rejected(CommandRejection::ProducerFenced);
            }
            Decision::Restart(entry.core.clone())
        }
    }
}

/// Answer `supervisor/command` until the session ends.
pub(crate) async fn serve(
    bus: &Bus,
    queryable: &ServerQueryable,
    state: &ExecutionState,
    control: &Control,
) {
    super::answer(
        bus,
        queryable,
        "command",
        |request: supervisor::command::Request| {
            let supervisor::command::Request::V0 { command } = request;
            let outcome = match decide(&command, &state.roster(), &state.board().snapshot()) {
                Decision::Rejected(reason) => {
                    tracing::debug!(?reason, "rejected a supervisor command");
                    CommandOutcome::Rejected { reason }
                }
                Decision::Stop => {
                    tracing::info!("stopping the execution on a supervisor command");
                    control.stop.cancel();
                    CommandOutcome::Accepted
                }
                Decision::Restart(key) => {
                    // The supervision loop owns the restart; a full queue means
                    // it is not consuming, which is not an accepted command.
                    match control.actions.try_send(SupervisorAction::Restart { key }) {
                        Ok(()) => CommandOutcome::Accepted,
                        Err(error) => {
                            tracing::warn!("failed to queue a restart: {error}");
                            CommandOutcome::Rejected {
                                reason: CommandRejection::NotRunning,
                            }
                        }
                    }
                }
            };
            supervisor::command::Reply::V0 { outcome }
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::identity::ProducerId;
    use phoxal_cli_core::runtime::{ParticipantKind, ProcessState, StartupRequirement};
    use phoxal_supervisor_api::{Name, ProcessKey as WireProcessKey};

    use super::*;
    use crate::SupervisorState;
    use crate::daemon::roster::tests::roster;

    fn producer(seed: u8) -> ProducerId {
        ProducerId::try_from(u128::from(seed)).expect("a producer id")
    }

    fn board_with_selected_processes() -> SupervisorState {
        let board = SupervisorState::new();
        for entry in roster().entries() {
            board.upsert_process(
                entry.core.clone(),
                ParticipantKind::Service,
                ProcessState::Starting,
                StartupRequirement::Required,
            );
        }
        board
    }

    fn restart(process: WireProcessKey, expected: u8) -> Command {
        Command::Restart {
            process,
            expected_producer: producer(expected),
        }
    }

    #[test]
    fn a_restart_matching_the_learned_producer_is_accepted() {
        let roster = roster();
        let board = board_with_selected_processes();
        let brain = roster
            .resolve(&WireProcessKey::Brain)
            .expect("the brain is selected")
            .core
            .clone();
        board.set_producer(&brain, producer(7));

        assert_eq!(
            decide(
                &restart(WireProcessKey::Brain, 7),
                &roster,
                &board.snapshot()
            ),
            Decision::Restart(brain)
        );
    }

    #[test]
    fn a_restart_fenced_on_a_producer_the_supervisor_never_learned_is_rejected() {
        let roster = roster();
        let board = board_with_selected_processes();
        // A freshly spawned incarnation has not opened its session, so it has
        // no producer and no expectation can match.
        assert_eq!(
            decide(
                &restart(WireProcessKey::Brain, 7),
                &roster,
                &board.snapshot()
            ),
            Decision::Rejected(CommandRejection::ProducerFenced)
        );

        // A superseded incarnation is the same rejection for the same reason.
        let brain = roster
            .resolve(&WireProcessKey::Brain)
            .expect("the brain")
            .core
            .clone();
        board.set_producer(&brain, producer(8));
        assert_eq!(
            decide(
                &restart(WireProcessKey::Brain, 7),
                &roster,
                &board.snapshot()
            ),
            Decision::Rejected(CommandRejection::ProducerFenced)
        );
    }

    #[test]
    fn a_restart_naming_no_selected_process_is_rejected_before_the_fence() {
        assert_eq!(
            decide(
                &restart(
                    WireProcessKey::Service {
                        id: Name::new("absent")
                    },
                    7
                ),
                &roster(),
                &board_with_selected_processes().snapshot()
            ),
            Decision::Rejected(CommandRejection::UnknownProcess)
        );
    }

    #[test]
    fn an_execution_on_its_way_out_accepts_nothing() {
        let roster = roster();
        let board = board_with_selected_processes();
        let brain = roster
            .resolve(&WireProcessKey::Brain)
            .expect("the brain")
            .core
            .clone();
        board.set_producer(&brain, producer(7));
        assert_eq!(
            decide(&Command::Stop, &roster, &board.snapshot()),
            Decision::Stop
        );

        for lifecycle in [
            ProjectLifecycle::Stopping,
            ProjectLifecycle::Stopped,
            ProjectLifecycle::Failed,
        ] {
            let board = board_with_selected_processes();
            board.set_producer(&brain, producer(7));
            board.set_lifecycle(lifecycle);
            let snapshot = board.snapshot();
            assert_eq!(
                decide(&Command::Stop, &roster, &snapshot),
                Decision::Rejected(CommandRejection::NotRunning),
                "{lifecycle:?}"
            );
            assert_eq!(
                decide(&restart(WireProcessKey::Brain, 7), &roster, &snapshot),
                Decision::Rejected(CommandRejection::NotRunning),
                "{lifecycle:?}"
            );
        }
    }
}
