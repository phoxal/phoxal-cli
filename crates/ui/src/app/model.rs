//! Typed application model. It contains presentation state and finite windows only.

use phoxal_cli_observation::AttachmentEpoch;

use crate::components::input::InputModel;
use crate::components::logs::LogsModel;
use crate::components::overview::OverviewModel;
use crate::components::runtimes::RuntimesModel;

use super::effect::AttachmentOutcome;
use super::route::FocusRoute;

#[derive(Debug, Clone, PartialEq)]
pub struct AppModel {
    pub epoch: Option<AttachmentEpoch>,
    /// Whether leaving this session is allowed to leave the execution running.
    /// A simulation session is not detachable: the client owns Webots, so `q`
    /// there ends the whole session (organization#978).
    pub detachable: bool,
    /// Whether a stop has already been sent. The session keeps rendering until
    /// the supervisor's own terminal snapshot arrives.
    pub stop_requested: bool,
    pub route: FocusRoute,
    pub overview: OverviewModel,
    pub runtimes: RuntimesModel,
    pub logs: LogsModel,
    pub input: InputModel,
    pub redraw_requested: bool,
    pub clear_requested: bool,
    pub exit: Option<AttachmentOutcome>,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            epoch: None,
            detachable: true,
            stop_requested: false,
            route: FocusRoute::default(),
            overview: OverviewModel::default(),
            runtimes: RuntimesModel::default(),
            logs: LogsModel::default(),
            input: InputModel::default(),
            redraw_requested: true,
            clear_requested: true,
            exit: None,
        }
    }
}
