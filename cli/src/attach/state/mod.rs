//! The retained client-side stores one attachment reads through.

pub(crate) mod health;
pub(crate) mod input;
pub(crate) mod logs;
pub(crate) mod motion;
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
    pub input: Arc<RwLock<input::InputStore>>,
    pub motion: Arc<RwLock<motion::MotionStore>>,
    pub health: Arc<RwLock<health::HealthStore>>,
}

impl Stores {
    pub fn new(epoch: phoxal_cli_observation::AttachmentEpoch) -> Self {
        Self {
            logs: Arc::new(RwLock::new(logs::LogStore::new(epoch))),
            runtimes: Arc::new(RwLock::new(runtimes::RuntimeStore::new(epoch))),
            input: Arc::new(RwLock::new(input::InputStore::default())),
            motion: Arc::new(RwLock::new(motion::MotionStore::default())),
            health: Arc::new(RwLock::new(health::HealthStore::default())),
        }
    }
}
