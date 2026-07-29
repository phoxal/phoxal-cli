//! Typed application model. It contains presentation state and finite windows only.

use phoxal_cli_observation::AttachmentEpoch;

use crate::components::bus::BusModel;
use crate::components::input::InputModel;
use crate::components::logs::LogsModel;
use crate::components::overview::OverviewModel;
use crate::components::runtimes::RuntimesModel;

use super::effect::AttachmentOutcome;
use super::route::FocusRoute;

#[derive(Debug, Clone, PartialEq)]
pub struct AppModel {
    pub epoch: Option<AttachmentEpoch>,
    pub route: FocusRoute,
    pub overview: OverviewModel,
    pub runtimes: RuntimesModel,
    pub logs: LogsModel,
    pub bus: BusModel,
    pub input: InputModel,
    pub redraw_requested: bool,
    pub clear_requested: bool,
    pub exit: Option<AttachmentOutcome>,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            epoch: None,
            route: FocusRoute::default(),
            overview: OverviewModel::default(),
            runtimes: RuntimesModel::default(),
            logs: LogsModel::default(),
            bus: BusModel::default(),
            input: InputModel::default(),
            redraw_requested: true,
            clear_requested: true,
            exit: None,
        }
    }
}
