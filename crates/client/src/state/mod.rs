pub(crate) mod bus;
pub(crate) mod device;
pub(crate) mod input;
pub(crate) mod logs;
pub(crate) mod motion;
pub(crate) mod processes;
pub(crate) mod runtimes;

use std::sync::Arc;

use tokio::sync::RwLock;

#[derive(Clone)]
pub(crate) struct Stores {
    pub logs: Arc<RwLock<logs::LogStore>>,
    pub bus: Arc<RwLock<bus::BusStore>>,
    pub runtimes: Arc<RwLock<runtimes::RuntimeStore>>,
    pub processes: Arc<RwLock<processes::ProcessStore>>,
    pub device: Arc<RwLock<device::DeviceStore>>,
    pub input: Arc<RwLock<input::InputStore>>,
    pub motion: Arc<RwLock<motion::MotionStore>>,
}

impl Stores {
    pub fn new(epoch: phoxal_cli_observation::AttachmentEpoch) -> Self {
        Self {
            logs: Arc::new(RwLock::new(logs::LogStore::new(epoch))),
            bus: Arc::new(RwLock::new(bus::BusStore::new(epoch))),
            runtimes: Arc::new(RwLock::new(runtimes::RuntimeStore::new(epoch))),
            processes: Arc::new(RwLock::new(processes::ProcessStore::default())),
            device: Arc::new(RwLock::new(device::DeviceStore::default())),
            input: Arc::new(RwLock::new(input::InputStore::default())),
            motion: Arc::new(RwLock::new(motion::MotionStore::default())),
        }
    }

    pub async fn replace_epoch(&self, epoch: phoxal_cli_observation::AttachmentEpoch) {
        self.logs.write().await.replace_epoch(epoch);
        self.bus.write().await.replace_epoch(epoch);
        self.runtimes.write().await.replace_epoch(epoch);
        self.processes.write().await.clear_graph();
        self.device.write().await.clear();
        self.input.write().await.clear();
        self.motion.write().await.clear();
    }
}
