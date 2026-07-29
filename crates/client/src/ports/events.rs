use phoxal_cli_observation::AttachmentEvent;
use tokio::sync::mpsc;

/// The single asynchronous stream of immutable attachment observations.
pub struct AttachmentEvents {
    pub(crate) receiver: mpsc::Receiver<AttachmentEvent>,
}

impl AttachmentEvents {
    pub async fn recv(&mut self) -> Option<AttachmentEvent> {
        self.receiver.recv().await
    }
}
