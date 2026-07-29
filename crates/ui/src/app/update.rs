//! Pure Elm-style update function.

use std::cmp::Ordering;

use phoxal_cli_core::session::ProjectLifecycle;
use phoxal_cli_observation::{
    AttachmentEvent, BusQuery, BusRead, LogAnchor, LogFilters, LogQuery, LogRead, QueryToken,
    RuntimeQuery, RuntimeRead, StoreChanged, StoreRevision, WindowDirection,
};
use tuirealm::event::{Key, KeyEvent, KeyModifiers};

use crate::components::bus::BusSort;
use crate::components::input::InputModel;
use crate::components::logs::{Editing, LogSourceFilter};
use crate::components::modal::ModalModel;

use super::effect::{AttachmentOutcome, DeviceId, Effect};
use super::id::{BusPanelId, InputPanelId, LogsPanelId, ModalId, PageId, PanelId, RuntimesPanelId};
use super::message::{BusMsg, LogsMsg, ModalMsg, Msg, NavigationMsg, RuntimesMsg};
use super::model::AppModel;
use super::route::{FocusRoute, cycle_panel};

const LOG_WINDOW_LIMIT: usize = 2_000;
const BUS_WINDOW_LIMIT: usize = 16_384;
const RUNTIME_WINDOW_LIMIT: usize = 4_096;

pub fn update(model: &mut AppModel, message: Msg) -> Vec<Effect> {
    let wake = matches!(message, Msg::Wake);
    let effects = match message {
        Msg::Wake => Vec::new(),
        Msg::Quit | Msg::Terminate => detach(model),
        Msg::Diagnostic(message) => {
            model.overview.push_diagnostic(message);
            Vec::new()
        }
        Msg::Client(event) => update_client(model, event),
        Msg::Navigate(message) => update_navigation(model, message),
        Msg::Logs(LogsMsg::Window(window)) => accept_logs(model, window),
        Msg::Bus(BusMsg::Window(window)) => accept_bus(model, window),
        Msg::Runtimes(RuntimesMsg::Window(window)) => accept_runtimes(model, window),
        Msg::Modal(message) => update_modal(model, message),
        Msg::Overview(_) | Msg::Input(_) => Vec::new(),
    };
    if !wake {
        model.redraw_requested = true;
    }
    effects
}

fn detach(model: &mut AppModel) -> Vec<Effect> {
    model.exit = Some(AttachmentOutcome::Detached);
    vec![Effect::Detach]
}

fn update_client(model: &mut AppModel, event: AttachmentEvent) -> Vec<Effect> {
    match event {
        AttachmentEvent::EpochChanged(epoch) => {
            model.epoch = Some(epoch);
            reset_queries(model);
            Vec::new()
        }
        AttachmentEvent::SupervisorChanged(supervisor) => {
            if matches!(
                supervisor.lifecycle,
                ProjectLifecycle::Stopped | ProjectLifecycle::Failed
            ) {
                model.exit = Some(AttachmentOutcome::ResidentStopped);
            }
            model.overview.supervisor = Some(supervisor);
            Vec::new()
        }
        AttachmentEvent::ProcessesChanged(processes) => {
            preserve_process_candidate(model, &processes);
            model.overview.processes = processes;
            Vec::new()
        }
        AttachmentEvent::DeviceChanged(devices) => {
            model.overview.devices = Some(devices);
            Vec::new()
        }
        AttachmentEvent::InputChanged(input) => {
            model.input.reconcile_authoritative((*input).clone());
            model.overview.input = Some(input);
            Vec::new()
        }
        AttachmentEvent::SourceHealthChanged(health) => {
            model.overview.source_health = Some(health);
            Vec::new()
        }
        AttachmentEvent::FreshnessChanged(freshness) => {
            model.overview.freshness = freshness;
            Vec::new()
        }
        AttachmentEvent::LogsChanged(changed) => invalidate_logs(model, changed),
        AttachmentEvent::BusChanged(changed) => invalidate_bus(model, changed),
        AttachmentEvent::RuntimesChanged(changed) => invalidate_runtimes(model, changed),
    }
}

fn reset_queries(model: &mut AppModel) {
    model.logs.rows.clear();
    model.logs.known_revision = StoreRevision(0);
    model.logs.dirty_revision = None;
    model.logs.in_flight = None;
    model.bus.rows.clear();
    model.bus.known_revision = StoreRevision(0);
    model.bus.dirty_revision = None;
    model.bus.in_flight = None;
    model.runtimes.rows.clear();
    model.runtimes.known_revision = StoreRevision(0);
    model.runtimes.dirty_revision = None;
    model.runtimes.in_flight = None;
}

fn preserve_process_candidate(
    model: &mut AppModel,
    processes: &phoxal_cli_observation::ProcessTable,
) {
    if model
        .runtimes
        .candidate
        .as_ref()
        .is_none_or(|candidate| !processes.contains_key(candidate))
    {
        model.runtimes.candidate = processes.keys().next().cloned();
    }
    if model
        .runtimes
        .detail
        .as_ref()
        .is_some_and(|detail| !processes.contains_key(detail))
    {
        model.runtimes.detail = None;
    }
}

fn newest(current: Option<StoreRevision>, incoming: StoreRevision) -> StoreRevision {
    current.map_or(incoming, |revision| revision.max(incoming))
}

fn invalidate_logs(model: &mut AppModel, changed: StoreChanged) -> Vec<Effect> {
    if model.epoch != Some(changed.epoch) {
        return Vec::new();
    }
    model.logs.dirty_revision = Some(newest(model.logs.dirty_revision, changed.revision));
    issue_logs_if_idle(model).into_iter().collect()
}

fn issue_logs_if_idle(model: &mut AppModel) -> Option<Effect> {
    let epoch = model.epoch?;
    if model.logs.in_flight.is_some() {
        return None;
    }
    let revision = model.logs.dirty_revision.take()?;
    model.logs.next_token = model.logs.next_token.wrapping_add(1);
    let token = QueryToken(model.logs.next_token);
    model.logs.in_flight = Some((epoch, revision, token));
    Some(Effect::ReadLogs(LogRead {
        epoch,
        observed_revision: revision,
        token,
        body: LogQuery {
            filters: LogFilters {
                participant: non_empty(&model.logs.participant),
                minimum_severity: model.logs.severity.minimum(),
                scope: None,
            },
            anchor: model
                .logs
                .pause_anchor
                .map_or(LogAnchor::Latest, LogAnchor::Before),
            direction: WindowDirection::Backward,
            limit: LOG_WINDOW_LIMIT,
        },
    }))
}

fn accept_logs(model: &mut AppModel, window: phoxal_cli_observation::LogWindow) -> Vec<Effect> {
    let Some((epoch, observed_revision, token)) = model.logs.in_flight else {
        return Vec::new();
    };
    if window.epoch != epoch || window.revision < observed_revision || window.token != token {
        return Vec::new();
    }
    model.logs.rows = window.rows.iter().cloned().collect();
    if !model.logs.text.is_empty() {
        let needle = model.logs.text.to_lowercase();
        model
            .logs
            .rows
            .retain(|row| row.text.to_lowercase().contains(&needle));
    }
    model.logs.known_revision = window.revision;
    model.logs.in_flight = None;
    model.logs.accepted_windows = model.logs.accepted_windows.saturating_add(1);
    issue_logs_if_idle(model).into_iter().collect()
}

fn invalidate_bus(model: &mut AppModel, changed: StoreChanged) -> Vec<Effect> {
    if model.epoch != Some(changed.epoch) {
        return Vec::new();
    }
    model.bus.dirty_revision = Some(newest(model.bus.dirty_revision, changed.revision));
    issue_bus_if_idle(model).into_iter().collect()
}

fn issue_bus_if_idle(model: &mut AppModel) -> Option<Effect> {
    let epoch = model.epoch?;
    if model.bus.in_flight.is_some() {
        return None;
    }
    let revision = model.bus.dirty_revision.take()?;
    model.bus.next_token = model.bus.next_token.wrapping_add(1);
    let token = QueryToken(model.bus.next_token);
    model.bus.in_flight = Some((epoch, revision, token));
    Some(Effect::ReadBus(BusRead {
        epoch,
        observed_revision: revision,
        token,
        body: BusQuery {
            topic: non_empty(&model.bus.filter),
            direction: WindowDirection::Forward,
            limit: BUS_WINDOW_LIMIT,
        },
    }))
}

fn accept_bus(model: &mut AppModel, window: phoxal_cli_observation::BusWindow) -> Vec<Effect> {
    let Some((epoch, observed_revision, token)) = model.bus.in_flight else {
        return Vec::new();
    };
    if window.epoch != epoch || window.revision < observed_revision || window.token != token {
        return Vec::new();
    }
    model.bus.rows = window.rows.iter().cloned().collect();
    sort_bus(model);
    model.bus.known_revision = window.revision;
    model.bus.in_flight = None;
    issue_bus_if_idle(model).into_iter().collect()
}

fn sort_bus(model: &mut AppModel) {
    match model.bus.sort {
        BusSort::Rate => model.bus.rows.sort_by(|left, right| {
            right
                .rate_hz
                .partial_cmp(&left.rate_hz)
                .unwrap_or(Ordering::Equal)
        }),
        BusSort::Topic => model
            .bus
            .rows
            .sort_by(|left, right| left.topic.cmp(&right.topic)),
        BusSort::Producer => model
            .bus
            .rows
            .sort_by(|left, right| left.participant.cmp(&right.participant)),
    }
}

fn invalidate_runtimes(model: &mut AppModel, changed: StoreChanged) -> Vec<Effect> {
    if model.epoch != Some(changed.epoch) {
        return Vec::new();
    }
    model.runtimes.dirty_revision = Some(newest(model.runtimes.dirty_revision, changed.revision));
    issue_runtimes_if_idle(model).into_iter().collect()
}

fn issue_runtimes_if_idle(model: &mut AppModel) -> Option<Effect> {
    let epoch = model.epoch?;
    if model.runtimes.in_flight.is_some() {
        return None;
    }
    let revision = model.runtimes.dirty_revision.take()?;
    model.runtimes.next_token = model.runtimes.next_token.wrapping_add(1);
    let token = QueryToken(model.runtimes.next_token);
    model.runtimes.in_flight = Some((epoch, revision, token));
    Some(Effect::ReadRuntimes(RuntimeRead {
        epoch,
        observed_revision: revision,
        token,
        body: RuntimeQuery {
            participant: model.runtimes.detail.as_ref().map(ToString::to_string),
            direction: WindowDirection::Forward,
            limit: RUNTIME_WINDOW_LIMIT,
        },
    }))
}

fn accept_runtimes(
    model: &mut AppModel,
    window: phoxal_cli_observation::RuntimeWindow,
) -> Vec<Effect> {
    let Some((epoch, observed_revision, token)) = model.runtimes.in_flight else {
        return Vec::new();
    };
    if window.epoch != epoch || window.revision < observed_revision || window.token != token {
        return Vec::new();
    }
    model.runtimes.rows = window.rows.iter().cloned().collect();
    model.runtimes.known_revision = window.revision;
    model.runtimes.in_flight = None;
    issue_runtimes_if_idle(model).into_iter().collect()
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn update_navigation(model: &mut AppModel, message: NavigationMsg) -> Vec<Effect> {
    match message {
        NavigationMsg::Resize { width, height } => {
            model.viewport = (width, height);
            Vec::new()
        }
        NavigationMsg::Page(page) => enter_page(model, page),
        NavigationMsg::OpenModal(modal) => {
            open_modal(model, modal);
            Vec::new()
        }
        NavigationMsg::CloseModal => {
            close_modal(model);
            Vec::new()
        }
        NavigationMsg::Key(key) => handle_key(model, key),
    }
}

fn handle_key(model: &mut AppModel, key: KeyEvent) -> Vec<Effect> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == Key::Char('c') {
        return detach(model);
    }
    if matches!(model.route, FocusRoute::Modal { .. }) {
        return handle_modal_key(model, key);
    }
    if let FocusRoute::Content { panel } = model.route.clone()
        && (model.logs.editing.is_some() || model.bus.editing)
    {
        return handle_content_key(model, panel, key);
    }
    if key.code == Key::Char('?') {
        open_modal(model, ModalId::Help);
        return Vec::new();
    }
    if key.code == Key::Char('i') {
        open_modal(model, ModalId::SessionInfo);
        return Vec::new();
    }
    if key.code == Key::Char('q') {
        return detach(model);
    }
    if key.code == Key::Char('S') {
        open_modal(model, ModalId::ConfirmStop);
        return Vec::new();
    }
    if let Key::Char(digit) = key.code {
        if let Some(page) = page_for_digit(digit) {
            return enter_page(model, page);
        }
    }
    match model.route.clone() {
        FocusRoute::Tabs { candidate } => match key.code {
            Key::Left | Key::Up | Key::BackTab => {
                model.route = FocusRoute::Tabs {
                    candidate: candidate.offset(-1),
                };
                Vec::new()
            }
            Key::Right | Key::Down | Key::Tab => {
                model.route = FocusRoute::Tabs {
                    candidate: candidate.offset(1),
                };
                Vec::new()
            }
            Key::Enter => enter_page(model, candidate),
            _ => Vec::new(),
        },
        FocusRoute::Panels { page, candidate } => match key.code {
            Key::Left | Key::Up | Key::BackTab => {
                model.route = FocusRoute::Panels {
                    page,
                    candidate: cycle_panel(page, candidate, -1),
                };
                Vec::new()
            }
            Key::Right | Key::Down | Key::Tab => {
                model.route = FocusRoute::Panels {
                    page,
                    candidate: cycle_panel(page, candidate, 1),
                };
                Vec::new()
            }
            Key::Enter => {
                if let Some(panel) = candidate {
                    model.route = FocusRoute::Content { panel };
                }
                Vec::new()
            }
            Key::Esc => {
                model.route = FocusRoute::Tabs { candidate: page };
                Vec::new()
            }
            _ => Vec::new(),
        },
        FocusRoute::Content { panel } => {
            if key.code == Key::Esc {
                model.route = FocusRoute::Panels {
                    page: panel.page(),
                    candidate: Some(panel),
                };
                return Vec::new();
            }
            handle_content_key(model, panel, key)
        }
        FocusRoute::Modal { .. } => unreachable!("modal handled above"),
    }
}

fn page_for_digit(digit: char) -> Option<PageId> {
    match digit {
        '1' => Some(PageId::Overview),
        '2' => Some(PageId::Runtimes),
        '3' => Some(PageId::Logs),
        '4' => Some(PageId::Bus),
        '5' => Some(PageId::Input),
        _ => None,
    }
}

fn enter_page(model: &mut AppModel, page: PageId) -> Vec<Effect> {
    model.modal = None;
    model.route = FocusRoute::panels(page);
    if page == PageId::Input {
        vec![Effect::InputRescan]
    } else {
        Vec::new()
    }
}

fn handle_content_key(model: &mut AppModel, panel: PanelId, key: KeyEvent) -> Vec<Effect> {
    match panel {
        PanelId::Runtimes(panel) => handle_runtimes_key(model, panel, key),
        PanelId::Logs(panel) => handle_logs_key(model, panel, key),
        PanelId::Bus(panel) => handle_bus_key(model, panel, key),
        PanelId::Input(panel) => handle_input_key(model, panel, key),
    }
}

fn handle_runtimes_key(model: &mut AppModel, panel: RuntimesPanelId, key: KeyEvent) -> Vec<Effect> {
    match panel {
        RuntimesPanelId::Processes => match key.code {
            Key::Up => move_process_candidate(model, -1),
            Key::Down => move_process_candidate(model, 1),
            Key::Enter => model.runtimes.detail.clone_from(&model.runtimes.candidate),
            Key::Char('r') => {
                if let Some(effect) = restart_effect(model) {
                    return vec![effect];
                }
            }
            Key::Char('l') => {
                if let Some(process) = &model.runtimes.candidate {
                    model.logs.participant = process.to_string();
                    model.logs.source = LogSourceFilter::Runtimes;
                    model.route = FocusRoute::Content {
                        panel: PanelId::Logs(LogsPanelId::Stream),
                    };
                    return refresh_logs(model);
                }
            }
            _ => {}
        },
        RuntimesPanelId::Performance => match key.code {
            Key::Up => model.runtimes.scroll = model.runtimes.scroll.saturating_sub(1),
            Key::Down => model.runtimes.scroll = model.runtimes.scroll.saturating_add(1),
            _ => {}
        },
    }
    Vec::new()
}

fn move_process_candidate(model: &mut AppModel, delta: isize) {
    let keys: Vec<_> = model.overview.processes.keys().cloned().collect();
    if keys.is_empty() {
        model.runtimes.candidate = None;
        return;
    }
    let index = model
        .runtimes
        .candidate
        .as_ref()
        .and_then(|candidate| keys.iter().position(|key| key == candidate))
        .unwrap_or(0);
    model.runtimes.candidate = keys
        .get(index.saturating_add_signed(delta).min(keys.len() - 1))
        .cloned();
}

fn restart_effect(model: &AppModel) -> Option<Effect> {
    let process = model.runtimes.candidate.clone()?;
    let expected_producer = model
        .overview
        .processes
        .get(&process)?
        .entry
        .status
        .producer?;
    Some(Effect::Restart {
        process,
        expected_producer,
    })
}

fn handle_logs_key(model: &mut AppModel, panel: LogsPanelId, key: KeyEvent) -> Vec<Effect> {
    if let Some(editing) = model.logs.editing {
        match key.code {
            Key::Esc | Key::Enter => model.logs.editing = None,
            Key::Backspace => match editing {
                Editing::Participant => {
                    model.logs.participant.pop();
                }
                Editing::Text => {
                    model.logs.text.pop();
                }
            },
            Key::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                match editing {
                    Editing::Participant => model.logs.participant.push(character),
                    Editing::Text => model.logs.text.push(character),
                }
            }
            _ => {}
        }
        return refresh_logs(model);
    }
    match panel {
        LogsPanelId::Filters => match key.code {
            Key::Left | Key::BackTab => {
                model.logs.control_candidate =
                    model.logs.control_candidate.checked_sub(1).unwrap_or(4);
            }
            Key::Right | Key::Tab => {
                model.logs.control_candidate = (model.logs.control_candidate + 1) % 5;
            }
            Key::Char('/') => {
                model.logs.control_candidate = 3;
                model.logs.editing = Some(Editing::Text);
            }
            Key::Char('f') => {
                model.logs.control_candidate = 1;
                model.logs.editing = Some(Editing::Participant);
            }
            Key::Char('s') => {
                model.logs.control_candidate = 2;
                model.logs.severity = model.logs.severity.cycle();
                return refresh_logs(model);
            }
            Key::Enter => match model.logs.control_candidate {
                0 => {
                    model.logs.source = model.logs.source.cycle();
                    return refresh_logs(model);
                }
                1 => model.logs.editing = Some(Editing::Participant),
                2 => {
                    model.logs.severity = model.logs.severity.cycle();
                    return refresh_logs(model);
                }
                3 => model.logs.editing = Some(Editing::Text),
                4 => toggle_follow(model),
                _ => {}
            },
            _ => {}
        },
        LogsPanelId::Stream => match key.code {
            Key::Up => {
                model.logs.follow = false;
                model.logs.pause_anchor = model.logs.rows.last().map(|row| row.event_time);
                model.logs.scroll = model.logs.scroll.saturating_add(1);
            }
            Key::Down => model.logs.scroll = model.logs.scroll.saturating_sub(1),
            Key::End => {
                model.logs.follow = true;
                model.logs.scroll = 0;
                model.logs.pause_anchor = None;
            }
            Key::Char(' ') => toggle_follow(model),
            _ => {}
        },
    }
    Vec::new()
}

fn toggle_follow(model: &mut AppModel) {
    model.logs.follow = !model.logs.follow;
    if model.logs.follow {
        model.logs.scroll = 0;
        model.logs.pause_anchor = None;
    } else {
        model.logs.pause_anchor = model.logs.rows.last().map(|row| row.event_time);
    }
}

fn refresh_logs(model: &mut AppModel) -> Vec<Effect> {
    model.logs.dirty_revision = Some(model.logs.known_revision);
    issue_logs_if_idle(model).into_iter().collect()
}

fn handle_bus_key(model: &mut AppModel, panel: BusPanelId, key: KeyEvent) -> Vec<Effect> {
    if panel == BusPanelId::Controls && model.bus.editing {
        match key.code {
            Key::Esc | Key::Enter => model.bus.editing = false,
            Key::Backspace => {
                model.bus.filter.pop();
            }
            Key::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                model.bus.filter.push(character);
            }
            _ => {}
        }
        return refresh_bus(model);
    }
    match panel {
        BusPanelId::Controls => match key.code {
            Key::Left | Key::BackTab => {
                model.bus.control_candidate =
                    model.bus.control_candidate.checked_sub(1).unwrap_or(2);
            }
            Key::Right | Key::Tab => {
                model.bus.control_candidate = (model.bus.control_candidate + 1) % 3;
            }
            Key::Char('/') => {
                model.bus.control_candidate = 0;
                model.bus.editing = true;
            }
            Key::Char('s') => {
                model.bus.sort = model.bus.sort.cycle();
                sort_bus(model);
            }
            Key::Char('a') => model.bus.show_internal = !model.bus.show_internal,
            Key::Enter => match model.bus.control_candidate {
                0 => model.bus.editing = true,
                1 => {
                    model.bus.sort = model.bus.sort.cycle();
                    sort_bus(model);
                }
                2 => model.bus.show_internal = !model.bus.show_internal,
                _ => {}
            },
            _ => {}
        },
        BusPanelId::Topics => match key.code {
            Key::Up => model.bus.scroll = model.bus.scroll.saturating_sub(1),
            Key::Down => model.bus.scroll = model.bus.scroll.saturating_add(1),
            _ => {}
        },
    }
    Vec::new()
}

fn refresh_bus(model: &mut AppModel) -> Vec<Effect> {
    model.bus.dirty_revision = Some(model.bus.known_revision);
    issue_bus_if_idle(model).into_iter().collect()
}

fn handle_input_key(model: &mut AppModel, panel: InputPanelId, key: KeyEvent) -> Vec<Effect> {
    if panel != InputPanelId::Devices {
        return Vec::new();
    }
    let devices = model
        .input
        .observation
        .as_ref()
        .map(|observation| observation.joypads.available.clone())
        .unwrap_or_default();
    match key.code {
        Key::Up => move_device_candidate(&mut model.input, &devices, -1),
        Key::Down => move_device_candidate(&mut model.input, &devices, 1),
        Key::Enter => {
            if let Some(candidate) = model.input.candidate.clone() {
                model.input.pending_selection = Some(candidate.clone());
                return vec![Effect::InputSelect(candidate)];
            }
        }
        Key::Char('e') => {
            model.input.pending_enabled = Some(true);
            return vec![Effect::InputEnable(true)];
        }
        Key::Char('x') => {
            model.input.pending_enabled = Some(false);
            return vec![Effect::InputEnable(false)];
        }
        Key::Char('r') => return vec![Effect::InputRescan],
        _ => {}
    }
    Vec::new()
}

fn move_device_candidate(
    input: &mut InputModel,
    devices: &[phoxal_cli_core::session::JoypadDevice],
    delta: isize,
) {
    if devices.is_empty() {
        input.candidate = None;
        return;
    }
    let index = input
        .candidate
        .as_ref()
        .and_then(|candidate| devices.iter().position(|device| device.id == candidate.0))
        .unwrap_or(0);
    input.candidate = devices
        .get(index.saturating_add_signed(delta).min(devices.len() - 1))
        .map(|device| DeviceId(device.id.clone()));
}

fn open_modal(model: &mut AppModel, id: ModalId) {
    let route = std::mem::take(&mut model.route);
    model.route = route.open_modal(id);
    model.modal = Some(ModalModel { id });
}

fn close_modal(model: &mut AppModel) {
    if let FocusRoute::Modal { return_to, .. } = std::mem::take(&mut model.route) {
        model.route = *return_to;
    }
    model.modal = None;
}

fn handle_modal_key(model: &mut AppModel, key: KeyEvent) -> Vec<Effect> {
    let modal = model.modal.as_ref().map(|modal| modal.id);
    match key.code {
        Key::Esc => {
            close_modal(model);
            Vec::new()
        }
        Key::Enter if modal == Some(ModalId::ConfirmStop) => {
            close_modal(model);
            vec![Effect::StopProject]
        }
        _ => Vec::new(),
    }
}

fn update_modal(model: &mut AppModel, message: ModalMsg) -> Vec<Effect> {
    match message {
        ModalMsg::Cancel => {
            close_modal(model);
            Vec::new()
        }
        ModalMsg::Confirm
            if model.modal.as_ref().map(|modal| modal.id) == Some(ModalId::ConfirmStop) =>
        {
            close_modal(model);
            vec![Effect::StopProject]
        }
        ModalMsg::Confirm => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Instant;

    use phoxal_cli_core::identity::ExecutionId;
    use phoxal_cli_core::session::{
        ParticipantKind, ParticipantState, ProcessDescriptor, ProcessEntry, ProcessKey,
        ProcessStatus, StartupRequirement,
    };
    use phoxal_cli_observation::{
        AttachmentEpoch, AttachmentEvent, LogWindow, ObservationWindow, ProcessObservation,
    };

    use super::*;

    fn epoch() -> AttachmentEpoch {
        AttachmentEpoch {
            supervisor_generation: 1,
            execution_id: ExecutionId::parse(&"0".repeat(ExecutionId::LEN))
                .expect("fixed execution id"),
            graph_generation: 1,
        }
    }

    #[test]
    fn one_thousand_log_invalidations_coalesce_while_read_is_in_flight() {
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Client(AttachmentEvent::EpochChanged(epoch())),
        );
        let mut effects = Vec::new();
        for revision in 1..=1_000 {
            effects.extend(update(
                &mut model,
                Msg::Client(AttachmentEvent::LogsChanged(StoreChanged {
                    epoch: epoch(),
                    revision: StoreRevision(revision),
                })),
            ));
        }
        assert_eq!(effects.len(), 1);
        let Effect::ReadLogs(first) = &effects[0] else {
            panic!("expected log read");
        };
        assert_eq!(first.body.limit, LOG_WINDOW_LIMIT);

        let follow_up = update(
            &mut model,
            Msg::Logs(LogsMsg::Window(LogWindow {
                epoch: epoch(),
                revision: StoreRevision(1),
                token: first.token,
                rows: Arc::from([]),
            })),
        );
        assert_eq!(follow_up.len(), 1);
        let Effect::ReadLogs(next) = &follow_up[0] else {
            panic!("expected coalesced follow-up read");
        };
        assert_eq!(next.observed_revision, StoreRevision(1_000));
        assert_eq!(model.logs.accepted_windows, 1);
    }

    #[test]
    fn stale_epoch_revision_and_token_windows_are_rejected() {
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Client(AttachmentEvent::EpochChanged(epoch())),
        );
        let effects = update(
            &mut model,
            Msg::Client(AttachmentEvent::LogsChanged(StoreChanged {
                epoch: epoch(),
                revision: StoreRevision(5),
            })),
        );
        let Effect::ReadLogs(read) = &effects[0] else {
            panic!("expected read");
        };
        for window in [
            ObservationWindow {
                epoch: AttachmentEpoch {
                    graph_generation: 2,
                    ..epoch()
                },
                revision: StoreRevision(5),
                token: read.token,
                rows: Arc::from([]),
            },
            ObservationWindow {
                epoch: epoch(),
                revision: StoreRevision(4),
                token: read.token,
                rows: Arc::from([]),
            },
            ObservationWindow {
                epoch: epoch(),
                revision: StoreRevision(5),
                token: QueryToken(read.token.0 + 1),
                rows: Arc::from([]),
            },
        ] {
            update(&mut model, Msg::Logs(LogsMsg::Window(window)));
            assert_eq!(model.logs.accepted_windows, 0);
            assert!(model.logs.in_flight.is_some());
        }
    }

    #[test]
    fn panel_actions_require_content_focus() {
        let mut model = AppModel {
            route: FocusRoute::Panels {
                page: PageId::Input,
                candidate: Some(PanelId::Input(InputPanelId::Devices)),
            },
            ..AppModel::default()
        };
        let effects = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('r').into())),
        );
        assert!(effects.is_empty());
    }

    #[test]
    fn log_filter_editing_precedes_global_shortcuts() {
        let mut model = AppModel {
            route: FocusRoute::Content {
                panel: PanelId::Logs(LogsPanelId::Filters),
            },
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('/').into())),
        );
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('q').into())),
        );
        assert_eq!(model.logs.text, "q");
        assert_eq!(model.exit, None);
    }

    #[test]
    fn log_follow_and_scroll_remain_page_local() {
        let mut model = AppModel {
            route: FocusRoute::Content {
                panel: PanelId::Logs(LogsPanelId::Stream),
            },
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Up.into())),
        );
        assert!(!model.logs.follow);
        assert_eq!(model.logs.scroll, 1);
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::End.into())),
        );
        assert!(model.logs.follow);
        assert_eq!(model.logs.scroll, 0);
    }

    #[test]
    fn modal_escape_restores_exact_route() {
        let return_to = FocusRoute::Content {
            panel: PanelId::Logs(LogsPanelId::Stream),
        };
        let mut model = AppModel {
            route: return_to.clone(),
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::OpenModal(ModalId::Help)),
        );
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Esc.into())),
        );
        assert_eq!(model.route, return_to);
    }

    #[test]
    fn runtime_candidate_tracks_identity_and_never_retargets_a_removed_row() {
        let alpha = ProcessKey::project("alpha");
        let beta = ProcessKey::project("beta");
        let mut processes = BTreeMap::from([
            (alpha.clone(), process(alpha.clone())),
            (beta.clone(), process(beta.clone())),
        ]);
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Client(AttachmentEvent::ProcessesChanged(Arc::new(
                processes.clone(),
            ))),
        );
        model.runtimes.candidate = Some(beta.clone());
        update(
            &mut model,
            Msg::Client(AttachmentEvent::ProcessesChanged(Arc::new(
                processes.clone(),
            ))),
        );
        assert_eq!(model.runtimes.candidate, Some(beta.clone()));

        processes.remove(&beta);
        update(
            &mut model,
            Msg::Client(AttachmentEvent::ProcessesChanged(Arc::new(processes))),
        );
        assert_eq!(model.runtimes.candidate, Some(alpha));
    }

    fn process(key: ProcessKey) -> ProcessObservation {
        ProcessObservation {
            key: key.clone(),
            entry: ProcessEntry {
                descriptor: ProcessDescriptor {
                    key,
                    kind: ParticipantKind::Service,
                    artifact: "service".to_string(),
                    owner: "test".to_string(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus::default(),
            },
            kind: ParticipantKind::Service,
            state: ParticipantState::Ready,
            present: Some(true),
            robot: None,
            started_at: Instant::now(),
            ended_at: None,
            first_ready_at: Some(Instant::now()),
            user_service: true,
        }
    }
}
