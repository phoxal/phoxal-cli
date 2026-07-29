use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use phoxal_cli_observation::{AttachmentEpoch, AttachmentEvent, Freshness, FreshnessSet};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct FreshnessState {
    epoch: AttachmentEpoch,
    due_by_source: BTreeMap<String, Instant>,
    values: FreshnessSet,
    dirty: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Scheduler {
    state: Arc<Mutex<FreshnessState>>,
    wake: Arc<Notify>,
}

impl Scheduler {
    pub(crate) fn new(epoch: AttachmentEpoch) -> Self {
        Self {
            state: Arc::new(Mutex::new(FreshnessState {
                epoch,
                due_by_source: BTreeMap::new(),
                values: FreshnessSet::new(),
                dirty: false,
            })),
            wake: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn reset(&self, epoch: AttachmentEpoch) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.epoch = epoch;
        state.due_by_source.clear();
        state.values.clear();
        state.dirty = true;
        drop(state);
        self.wake.notify_one();
    }

    pub(crate) fn refresh(&self, epoch: AttachmentEpoch, source: impl Into<String>, ttl: Duration) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.epoch != epoch {
            return;
        }
        let source = source.into();
        let previous_earliest = state.due_by_source.values().copied().min();
        let due = Instant::now() + ttl;
        state.due_by_source.insert(source.clone(), due);
        let became_fresh = state.values.insert(source, Freshness::Fresh) != Some(Freshness::Fresh);
        if became_fresh {
            state.dirty = true;
        }
        drop(state);
        if became_fresh || previous_earliest.is_none_or(|earliest| due < earliest) {
            self.wake.notify_one();
        }
    }
}

pub(crate) async fn run(
    scheduler: Scheduler,
    events: mpsc::Sender<AttachmentEvent>,
    cancellation: CancellationToken,
) {
    loop {
        let notified = scheduler.wake.notified();
        let (event, earliest) = {
            let mut state = scheduler
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let stale = state
                .due_by_source
                .iter()
                .filter(|(_, due)| **due <= now)
                .map(|(source, _)| source.clone())
                .collect::<Vec<_>>();
            for source in stale {
                state.due_by_source.remove(&source);
                if state.values.insert(source, Freshness::Stale) != Some(Freshness::Stale) {
                    state.dirty = true;
                }
            }
            let event = state.dirty.then(|| {
                state.dirty = false;
                AttachmentEvent::FreshnessChanged {
                    epoch: state.epoch,
                    values: state.values.clone(),
                }
            });
            let earliest = state.due_by_source.values().copied().min();
            (event, earliest)
        };

        if let Some(event) = event {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                sent = events.send(event) => {
                    if sent.is_err() {
                        return;
                    }
                }
            }
            continue;
        }

        match earliest {
            Some(deadline) => {
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    () = notified => {}
                    _ = tokio::time::sleep_until(deadline.into()) => {}
                }
            }
            None => {
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    () = notified => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::identity::ExecutionId;

    use super::*;

    fn epoch(graph_generation: u64) -> AttachmentEpoch {
        AttachmentEpoch {
            supervisor_generation: 1,
            execution_id: ExecutionId::mint(),
            graph_generation,
        }
    }

    #[tokio::test]
    async fn scheduler_coalesces_refreshes_and_fences_epochs() {
        let old = epoch(1);
        let scheduler = Scheduler::new(old);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run(scheduler.clone(), event_tx, cancellation.clone()));

        for _ in 0..100_000 {
            scheduler.refresh(old, "device", Duration::from_millis(10));
        }
        let fresh = event_rx.recv().await.expect("fresh event");
        assert!(matches!(
            fresh,
            AttachmentEvent::FreshnessChanged { epoch, ref values }
                if epoch == old && values.get("device") == Some(&Freshness::Fresh)
        ));

        let new = AttachmentEpoch {
            graph_generation: 2,
            ..old
        };
        scheduler.reset(new);
        scheduler.refresh(old, "logs", Duration::from_secs(1));
        let reset = event_rx.recv().await.expect("reset event");
        assert!(matches!(
            reset,
            AttachmentEvent::FreshnessChanged { epoch, ref values }
                if epoch == new && values.is_empty()
        ));

        cancellation.cancel();
        task.await.unwrap();
    }
}
