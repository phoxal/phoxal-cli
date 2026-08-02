use tokio::sync::mpsc;

use crate::ports::input::InputCommand;
use crate::ports::{AttachmentEvents, InputCommands, LogReader, RuntimeReader};
use crate::state::Stores;
use crate::supervisor::SupervisorCommands;

pub struct AttachmentPorts {
    pub events: AttachmentEvents,
    pub supervisor_commands: SupervisorCommands,
    pub input_commands: InputCommands,
    pub logs: LogReader,
    pub runtimes: RuntimeReader,
}

impl AttachmentPorts {
    pub(crate) fn new(
        receiver: mpsc::Receiver<phoxal_cli_observation::AttachmentEvent>,
        supervisor_commands: SupervisorCommands,
        input_sender: mpsc::Sender<InputCommand>,
        stores: &Stores,
    ) -> Self {
        Self {
            events: AttachmentEvents { receiver },
            supervisor_commands,
            input_commands: InputCommands {
                sender: input_sender,
            },
            logs: LogReader {
                store: stores.logs.clone(),
            },
            runtimes: RuntimeReader {
                store: stores.runtimes.clone(),
            },
        }
    }
}
