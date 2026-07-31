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

    pub fn try_recv(&mut self) -> Result<AttachmentEvent, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn close(&mut self) {
        self.receiver.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_recv_exposes_events_already_seeded_by_attachment_setup() {
        let (sender, receiver) = mpsc::channel(1);
        sender
            .try_send(AttachmentEvent::ConnectionChanged(
                phoxal_cli_observation::ConnectionObservation::Connected,
            ))
            .unwrap();
        let mut events = AttachmentEvents { receiver };
        assert!(matches!(
            events.try_recv(),
            Ok(AttachmentEvent::ConnectionChanged(
                phoxal_cli_observation::ConnectionObservation::Connected
            ))
        ));
    }
}
