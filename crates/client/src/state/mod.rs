pub(crate) mod input;
pub(crate) mod logs;
pub(crate) mod motion;
pub(crate) mod processes;
pub(crate) mod runtimes;

use std::sync::Arc;

use tokio::sync::RwLock;

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    value
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[derive(Clone)]
pub(crate) struct Stores {
    pub logs: Arc<RwLock<logs::LogStore>>,
    pub runtimes: Arc<RwLock<runtimes::RuntimeStore>>,
    pub processes: Arc<RwLock<processes::ProcessStore>>,
    pub input: Arc<RwLock<input::InputStore>>,
    pub motion: Arc<RwLock<motion::MotionStore>>,
}

impl Stores {
    pub fn new(epoch: phoxal_cli_observation::AttachmentEpoch) -> Self {
        Self {
            logs: Arc::new(RwLock::new(logs::LogStore::new(epoch))),
            runtimes: Arc::new(RwLock::new(runtimes::RuntimeStore::new(epoch))),
            processes: Arc::new(RwLock::new(processes::ProcessStore::default())),
            input: Arc::new(RwLock::new(input::InputStore::default())),
            motion: Arc::new(RwLock::new(motion::MotionStore::default())),
        }
    }

    pub async fn replace_epoch(&self, epoch: phoxal_cli_observation::AttachmentEpoch) {
        self.logs.write().await.replace_epoch(epoch);
        self.runtimes.write().await.replace_epoch(epoch);
        self.processes.write().await.clear_graph();
        self.input.write().await.clear();
        self.motion.write().await.clear();
    }
}
