use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use phoxal_cli_observation::{AttachmentEvent, Freshness, FreshnessSet};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(crate) struct Deadline {
    source: String,
    due: Instant,
}

pub(crate) type DeadlineSender = mpsc::UnboundedSender<Deadline>;

pub(crate) fn refresh(sender: &DeadlineSender, source: impl Into<String>, ttl: Duration) {
    let _ = sender.send(Deadline {
        source: source.into(),
        due: Instant::now() + ttl,
    });
}

pub(crate) async fn run(
    mut deadlines: mpsc::UnboundedReceiver<Deadline>,
    events: mpsc::Sender<AttachmentEvent>,
    cancellation: CancellationToken,
) {
    let mut due_by_source = BTreeMap::<String, Instant>::new();
    let mut current = FreshnessSet::new();
    loop {
        let earliest = due_by_source.values().copied().min();
        let received = match earliest {
            Some(deadline) => {
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    received = deadlines.recv() => Some(received),
                    _ = tokio::time::sleep_until(deadline.into()) => None,
                }
            }
            None => {
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    received = deadlines.recv() => Some(received),
                }
            }
        };

        match received {
            Some(Some(deadline)) => {
                due_by_source.insert(deadline.source.clone(), deadline.due);
                if current.insert(deadline.source, Freshness::Fresh) != Some(Freshness::Fresh)
                    && events
                        .send(AttachmentEvent::FreshnessChanged(current.clone()))
                        .await
                        .is_err()
                {
                    return;
                }
            }
            Some(None) => return,
            None => {
                let now = Instant::now();
                let stale = due_by_source
                    .iter()
                    .filter(|(_, due)| **due <= now)
                    .map(|(source, _)| source.clone())
                    .collect::<Vec<_>>();
                let mut changed = false;
                for source in stale {
                    due_by_source.remove(&source);
                    changed |= current.insert(source, Freshness::Stale) != Some(Freshness::Stale);
                }
                if changed
                    && events
                        .send(AttachmentEvent::FreshnessChanged(current.clone()))
                        .await
                        .is_err()
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scheduler_coalesces_due_sources_and_refreshes_independently() {
        let (deadline_tx, deadline_rx) = mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run(deadline_rx, event_tx, cancellation.clone()));

        refresh(&deadline_tx, "device", Duration::from_millis(10));
        refresh(&deadline_tx, "logs", Duration::from_secs(1));
        let _ = event_rx.recv().await;
        let _ = event_rx.recv().await;
        let stale = tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
            .await
            .expect("device deadline should fire")
            .expect("freshness event stream should remain open");
        let AttachmentEvent::FreshnessChanged(stale) = stale else {
            panic!("scheduler emitted a non-freshness event");
        };
        assert_eq!(stale.get("device"), Some(&Freshness::Stale));
        assert_eq!(stale.get("logs"), Some(&Freshness::Fresh));

        cancellation.cancel();
        task.await.unwrap();
    }
}
