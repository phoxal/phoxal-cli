use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use phoxal_cli_core::identity::ProducerId;
use phoxal_cli_protocol::limits::MAX_RECENT_COMMAND_REPLIES;
use phoxal_cli_protocol::{
    CommandAction, CommandError, CommandReply, CommandRequest, CommandSessionId,
};

use crate::SupervisorAction;

use super::server::ServerState;

pub(super) const COMMAND_SESSION_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_COMMAND_SESSIONS: usize = 64;

#[derive(Debug)]
pub(super) struct CommandSessionState {
    highest_processed: u64,
    recent_replies: BTreeMap<u64, CommandReply>,
    pub(super) last_used: Instant,
}

#[derive(Debug, Default)]
pub(super) struct CommandSessions {
    pub(super) active: HashMap<CommandSessionId, CommandSessionState>,
    /// Fence accepted restarts across reconnecting command sessions until the
    /// resident advances the producer identity.
    pending_restarts: HashMap<phoxal_cli_core::runtime::ProcessKey, ProducerId>,
}

pub(super) fn process_command(
    state: &ServerState,
    connection_session: CommandSessionId,
    request: CommandRequest,
) -> CommandReply {
    let current = state.board.supervisor_snapshot();
    if request.supervisor_generation != current.supervisor_generation {
        return CommandReply::rejected(CommandError::StaleSupervisorGeneration);
    }
    if request.key.session != connection_session {
        return CommandReply::rejected(CommandError::InvalidSession);
    }

    let mut sessions = state
        .sessions
        .lock()
        .expect("command sessions mutex poisoned");
    expire_sessions(&mut sessions);
    // A pending restart settles only after the board reports the producer the
    // supervisor pre-minted for that restart.
    sessions.pending_restarts.retain(|key, expected| {
        current
            .processes
            .get(key)
            .and_then(|entry| entry.status.producer)
            == Some(*expected)
    });
    {
        let Some(session) = sessions.active.get_mut(&connection_session) else {
            return CommandReply::rejected(CommandError::InvalidSession);
        };
        session.last_used = Instant::now();
        if request.key.sequence <= session.highest_processed {
            return session
                .recent_replies
                .get(&request.key.sequence)
                .cloned()
                .unwrap_or_else(|| CommandReply::rejected(CommandError::AlreadyProcessed));
        }
        if request.key.sequence != session.highest_processed.saturating_add(1) {
            return CommandReply::rejected(CommandError::OutOfOrder);
        }
    }

    let reply = match request.action {
        CommandAction::Restart {
            process,
            expected_producer,
        } => match current.processes.get(&process) {
            None => CommandReply::rejected(CommandError::UnknownProcess),
            Some(entry) if entry.status.producer != Some(expected_producer) => {
                CommandReply::rejected(CommandError::SupersededProducer)
            }
            Some(_) if sessions.pending_restarts.get(&process) == Some(&expected_producer) => {
                CommandReply::rejected(CommandError::AlreadyProcessed)
            }
            Some(_) => match state.actions.try_send(SupervisorAction::Restart {
                key: process.clone(),
            }) {
                Ok(()) => {
                    sessions.pending_restarts.insert(process, expected_producer);
                    CommandReply::accepted()
                }
                Err(_) => CommandReply::rejected(CommandError::SupervisorUnavailable),
            },
        },
        CommandAction::Shutdown => {
            state.supervisor_token.cancel();
            CommandReply::accepted()
        }
    };
    let session = sessions
        .active
        .get_mut(&connection_session)
        .expect("validated command session disappeared while mutex held");
    session.highest_processed = request.key.sequence;
    session
        .recent_replies
        .insert(request.key.sequence, reply.clone());
    while session.recent_replies.len() > MAX_RECENT_COMMAND_REPLIES {
        let Some(oldest) = session.recent_replies.keys().next().copied() else {
            break;
        };
        session.recent_replies.remove(&oldest);
    }
    reply
}

pub(super) fn issue_or_resume_session(
    sessions: &Arc<Mutex<CommandSessions>>,
    requested: Option<CommandSessionId>,
) -> CommandSessionId {
    let mut sessions = sessions.lock().expect("command sessions mutex poisoned");
    expire_sessions(&mut sessions);
    if let Some(requested) = requested
        && let Some(session) = sessions.active.get_mut(&requested)
    {
        session.last_used = Instant::now();
        return requested;
    }
    while sessions.active.len() >= MAX_COMMAND_SESSIONS {
        let Some(oldest) = sessions
            .active
            .iter()
            .min_by_key(|(_, state)| state.last_used)
            .map(|(id, _)| *id)
        else {
            break;
        };
        sessions.active.remove(&oldest);
    }
    loop {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).expect("operating-system CSPRNG unavailable");
        let id = CommandSessionId(bytes);
        if let std::collections::hash_map::Entry::Vacant(entry) = sessions.active.entry(id) {
            entry.insert(CommandSessionState {
                highest_processed: 0,
                recent_replies: BTreeMap::new(),
                last_used: Instant::now(),
            });
            return id;
        }
    }
}

fn expire_sessions(sessions: &mut CommandSessions) {
    sessions
        .active
        .retain(|_, session| session.last_used.elapsed() < COMMAND_SESSION_TTL);
}
