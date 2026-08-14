//! Pure Elm-style update function.

use std::collections::BTreeMap;
use std::time::SystemTime;

use phoxal_cli_observation::{
    AttachmentEvent, LogAnchor, LogFilters, LogQuery, LogRead, LogRow, ProcessTable, QueryToken,
    RuntimeQuery, RuntimeRead, RuntimeRow, StoreChanged, StoreRevision, WindowDirection,
};
use phoxal_client::supervisor::execution::Lifecycle;
use tuirealm::event::{Key, KeyEvent, KeyModifiers};

use crate::components::input::InputModel;
use crate::components::logs::{Editing, LogSourceFilter};

use super::effect::{AttachmentOutcome, DeviceId, Effect};
use super::id::{InputPanelId, LogsPanelId, ModalId, PageId, PanelId, RuntimesPanelId};
use super::message::{LogsMsg, Msg, NavigationMsg, RuntimesMsg};
use super::model::AppModel;
use super::route::{FocusRoute, cycle_panel};

const LOG_WINDOW_LIMIT: usize = 2_000;
const RUNTIME_WINDOW_LIMIT: usize = 4_096;

pub fn update(model: &mut AppModel, message: Msg) -> Vec<Effect> {
    let wake = matches!(message, Msg::Wake);
    let effects = match message {
        Msg::Wake => Vec::new(),
        Msg::Terminate => detach(model),
        Msg::Interrupt => interrupt(model),
        Msg::Diagnostic(message) => {
            model.overview.push_diagnostic(message);
            Vec::new()
        }
        Msg::Client(event) => update_client(model, event),
        Msg::Navigate(message) => update_navigation(model, message),
        Msg::Logs(LogsMsg::Window(window)) => accept_logs(model, window),
        Msg::Runtimes(RuntimesMsg::Window(window)) => accept_runtimes(model, window),
    };
    if !wake {
        model.redraw_requested = true;
    }
    effects
}

/// Leave the session without touching the execution.
///
/// In a simulation session there is nothing to leave behind: the client owns
/// Webots, so detaching would strand a simulator with no operator. `q` there
/// means "end the session", which is a stop.
fn detach(model: &mut AppModel) -> Vec<Effect> {
    if !model.detachable {
        return request_stop(model);
    }
    model.exit = Some(AttachmentOutcome::Detached);
    Vec::new()
}

/// Ask the supervisor to end the execution, then keep rendering.
///
/// The session does NOT exit here. It exits when the supervisor's own terminal
/// snapshot arrives, or when its identity token is lost - so an operator sees
/// the graph shutting down rather than a terminal that closed on a hope.
fn request_stop(model: &mut AppModel) -> Vec<Effect> {
    close_modal(model);
    if model.stop_requested {
        return Vec::new();
    }
    model.stop_requested = true;
    vec![Effect::StopProject]
}

/// What Ctrl+C means.
///
/// The first one opens the confirmation and sends nothing: an interrupt in a
/// terminal that is driving a robot must never be one keystroke away from
/// stopping it by reflex. The second one, with the confirmation already up, is
/// the confirmation.
fn interrupt(model: &mut AppModel) -> Vec<Effect> {
    if model.route.modal() == Some(ModalId::ConfirmStop) {
        return request_stop(model);
    }
    open_modal(model, ModalId::ConfirmStop);
    Vec::new()
}

fn update_client(model: &mut AppModel, event: AttachmentEvent) -> Vec<Effect> {
    match event {
        AttachmentEvent::EpochChanged(epoch) => {
            model.epoch = Some(epoch);
            reset_queries(model);
            Vec::new()
        }
        AttachmentEvent::ConnectionChanged(connection) => {
            // A connection loss never overrides a supervisor-reported failure
            // or stop that already arrived (see the ordering tests below); it
            // only fills in an exit when the client never saw a terminal
            // snapshot at all. `reason` is deliberately `None` here - a
            // transport loss is not a supervisor-reported cause, so the
            // caller falls through to its own supervisor.log pointer instead
            // of surfacing this transport-level text as if the supervisor had
            // explained itself.
            if matches!(
                &connection,
                phoxal_cli_observation::ConnectionObservation::Lost { .. }
            ) && model.exit.is_none()
            {
                model.exit = Some(AttachmentOutcome::ExecutionFailed { reason: None });
            }
            model.overview.connection = Some(connection);
            Vec::new()
        }
        AttachmentEvent::SupervisorChanged(supervisor) => {
            model.exit = match supervisor.lifecycle {
                Lifecycle::Stopped => Some(AttachmentOutcome::ExecutionStopped),
                Lifecycle::Failed => Some(AttachmentOutcome::ExecutionFailed {
                    reason: supervisor.failure.clone(),
                }),
                _ => model.exit.clone(),
            };
            model.overview.supervisor = Some(supervisor);
            Vec::new()
        }
        AttachmentEvent::ProcessesChanged {
            epoch,
            values: processes,
        } => {
            if model.epoch != Some(epoch) {
                return Vec::new();
            }
            let detail_cleared = preserve_process_candidate(model, &processes);
            model.overview.processes = processes;
            if detail_cleared {
                refresh_runtimes(model)
            } else {
                Vec::new()
            }
        }
        AttachmentEvent::InputChanged {
            epoch,
            values: input,
        } => {
            if model.epoch != Some(epoch) {
                return Vec::new();
            }
            model.input.reconcile_authoritative((*input).clone());
            model.overview.input = Some(input);
            Vec::new()
        }
        AttachmentEvent::SourceHealthChanged {
            epoch,
            values: health,
        } => {
            if model.epoch != Some(epoch) {
                return Vec::new();
            }
            model.overview.source_health = Some(health);
            Vec::new()
        }
        AttachmentEvent::LogsChanged(changed) => invalidate_logs(model, changed),
        AttachmentEvent::RuntimesChanged(changed) => invalidate_runtimes(model, changed),
    }
}

fn reset_queries(model: &mut AppModel) {
    model.logs.rows.clear();
    model.logs.known_revision = StoreRevision(0);
    model.logs.dirty_revision = None;
    model.logs.in_flight = None;
    model.runtimes.rows.clear();
    model.runtimes.known_revision = StoreRevision(0);
    model.runtimes.dirty_revision = None;
    model.runtimes.in_flight = None;
}

fn preserve_process_candidate(
    model: &mut AppModel,
    processes: &phoxal_cli_observation::ProcessTable,
) -> bool {
    if model
        .runtimes
        .candidate
        .as_ref()
        .is_none_or(|candidate| !processes.contains_key(candidate))
    {
        model.runtimes.candidate = processes.keys().next().cloned();
    }
    let detail_cleared = model
        .runtimes
        .detail
        .as_ref()
        .is_some_and(|detail| !processes.contains_key(detail));
    if detail_cleared {
        model.runtimes.detail = None;
    }
    detail_cleared
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
            },
            anchor: model
                .logs
                .pause_anchor
                .map_or(LogAnchor::Latest, LogAnchor::Before),
            direction: WindowDirection::Forward,
            limit: LOG_WINDOW_LIMIT,
        },
    }))
}

fn accept_logs(model: &mut AppModel, window: phoxal_cli_observation::LogWindow) -> Vec<Effect> {
    let Some((epoch, observed_revision, token)) = model.logs.in_flight else {
        return Vec::new();
    };
    if window.epoch != epoch || window.revision < observed_revision || window.token != token {
        model.logs.in_flight = None;
        model.logs.dirty_revision = Some(newest(model.logs.dirty_revision, observed_revision));
        return issue_logs_if_idle(model).into_iter().collect();
    }
    model.logs.rows = window
        .rows
        .iter()
        .filter(|row| log_matches_source(model.logs.source, row, &model.overview.processes))
        .cloned()
        .collect();
    if !model.logs.text.is_empty() {
        let needle = model.logs.text.to_lowercase();
        model
            .logs
            .rows
            .retain(|row| row.text.to_lowercase().contains(&needle));
    }
    model.logs.known_revision = window.revision;
    model.logs.in_flight = None;
    issue_logs_if_idle(model).into_iter().collect()
}

fn log_matches_source(source: LogSourceFilter, row: &LogRow, processes: &ProcessTable) -> bool {
    if source == LogSourceFilter::All {
        return true;
    }
    // Every row in the snapshot is a supervised participant, so a log whose
    // participant matches one came from the graph; anything else came from the
    // supervisor itself.
    let runtime = processes
        .keys()
        .any(|key| participant_id(key) == row.participant);
    match source {
        LogSourceFilter::All => true,
        LogSourceFilter::Runtimes => runtime,
        LogSourceFilter::System => !runtime,
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
            participant: model.runtimes.detail.as_ref().map(participant_id),
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
        model.runtimes.in_flight = None;
        model.runtimes.dirty_revision =
            Some(newest(model.runtimes.dirty_revision, observed_revision));
        return issue_runtimes_if_idle(model).into_iter().collect();
    }
    let mut newest_rows = BTreeMap::<String, RuntimeRow>::new();
    for row in window.rows.iter() {
        newest_rows.insert(row.sample.participant_id().to_string(), row.clone());
    }
    model.runtimes.rows = newest_rows.into_values().collect();
    model.runtimes.known_revision = window.revision;
    model.runtimes.in_flight = None;
    issue_runtimes_if_idle(model).into_iter().collect()
}

/// The participant id a supervisor log or telemetry record is stamped with,
/// for the process key that denotes the same participant.
fn participant_id(key: &phoxal_runtime_contract::identity::ParticipantId) -> String {
    key.to_string()
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn update_navigation(model: &mut AppModel, message: NavigationMsg) -> Vec<Effect> {
    match message {
        NavigationMsg::Refresh { clear } => {
            model.clear_requested |= clear;
            Vec::new()
        }
        NavigationMsg::Key(key) => handle_key(model, key),
    }
}

fn handle_key(model: &mut AppModel, key: KeyEvent) -> Vec<Effect> {
    // Ctrl+C is the stop gesture, and it takes two: the first opens the
    // confirmation and sends nothing, the second confirms.
    // It is handled before the modal branch so the second one is not consumed
    // as an ordinary modal key.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == Key::Char('c') {
        return interrupt(model);
    }
    if matches!(model.route, FocusRoute::Modal { .. }) {
        return handle_modal_key(model, key);
    }
    if let FocusRoute::Content { panel } = model.route.clone()
        && model.logs.editing.is_some()
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
    if let Key::Char(digit) = key.code
        && let Some(page) = page_for_digit(digit)
    {
        return enter_page(model, page);
    }
    match model.route.clone() {
        FocusRoute::Tabs { page, candidate } => match key.code {
            Key::Left | Key::Up | Key::BackTab => {
                model.route = FocusRoute::Tabs {
                    page,
                    candidate: candidate.offset(-1),
                };
                Vec::new()
            }
            Key::Right | Key::Down | Key::Tab => {
                model.route = FocusRoute::Tabs {
                    page,
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
                model.route = FocusRoute::Tabs {
                    page,
                    candidate: page,
                };
                Vec::new()
            }
            _ => Vec::new(),
        },
        FocusRoute::Content { panel } => {
            if key.code == Key::Esc {
                let mut effects = Vec::new();
                if matches!(panel, PanelId::Runtimes(_)) {
                    model.runtimes.detail = None;
                    effects = refresh_runtimes(model);
                }
                model.route = FocusRoute::Panels {
                    page: panel.page(),
                    candidate: Some(panel),
                };
                return effects;
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
        '5' => Some(PageId::Input),
        _ => None,
    }
}

fn enter_page(model: &mut AppModel, page: PageId) -> Vec<Effect> {
    model.runtimes.detail = None;
    model.route = FocusRoute::panels(page);
    let mut effects = if page == PageId::Runtimes {
        refresh_runtimes(model)
    } else {
        Vec::new()
    };
    if page == PageId::Input {
        effects.push(Effect::InputRescan);
    }
    effects
}

fn handle_content_key(model: &mut AppModel, panel: PanelId, key: KeyEvent) -> Vec<Effect> {
    match panel {
        PanelId::Runtimes(panel) => handle_runtimes_key(model, panel, key),
        PanelId::Logs(panel) => handle_logs_key(model, panel, key),
        PanelId::Input(panel) => handle_input_key(model, panel, key),
    }
}

fn handle_runtimes_key(model: &mut AppModel, panel: RuntimesPanelId, key: KeyEvent) -> Vec<Effect> {
    match panel {
        RuntimesPanelId::Processes => match key.code {
            Key::Up => move_process_candidate(model, -1),
            Key::Down => move_process_candidate(model, 1),
            Key::Enter => {
                model.runtimes.detail.clone_from(&model.runtimes.candidate);
                model.route = FocusRoute::Content {
                    panel: PanelId::Runtimes(RuntimesPanelId::Performance),
                };
                return refresh_runtimes(model);
            }
            Key::Char('r') => match restart_effect(model) {
                Ok(effect) => return vec![effect],
                Err(message) => model.overview.push_diagnostic(message),
            },
            Key::Char('l') => {
                if let Some(process) = &model.runtimes.candidate {
                    model.logs.participant = process.to_string();
                    model.logs.source = LogSourceFilter::Runtimes;
                    model.logs.severity = crate::components::logs::SeverityFilter::All;
                    model.logs.text.clear();
                    model.logs.scroll = 0;
                    model.logs.follow = true;
                    model.logs.pause_anchor = None;
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

fn restart_effect(model: &AppModel) -> Result<Effect, String> {
    let process = model
        .runtimes
        .candidate
        .clone()
        .ok_or_else(|| "no runtime is selected for restart".to_string())?;
    let expected_producer = model
        .overview
        .processes
        .get(&process)
        .ok_or_else(|| format!("selected process `{process}` is no longer present"))?
        .row
        .producer
        .ok_or_else(|| format!("process `{process}` has no restartable producer"))?;
    Ok(Effect::Restart {
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
                model.logs.pause_anchor = Some(newest_log_time(&model.logs.rows));
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
        model.logs.pause_anchor = Some(newest_log_time(&model.logs.rows));
    }
}

fn newest_log_time(rows: &[LogRow]) -> SystemTime {
    rows.iter()
        .map(|row| row.event_time)
        .max()
        .and_then(|time| time.checked_add(std::time::Duration::from_nanos(1)))
        .unwrap_or_else(SystemTime::now)
}

fn refresh_runtimes(model: &mut AppModel) -> Vec<Effect> {
    model.runtimes.dirty_revision = Some(model.runtimes.known_revision);
    issue_runtimes_if_idle(model).into_iter().collect()
}

fn refresh_logs(model: &mut AppModel) -> Vec<Effect> {
    model.logs.dirty_revision = Some(model.logs.known_revision);
    issue_logs_if_idle(model).into_iter().collect()
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
    let input_fresh = model.overview.source_health.as_ref().and_then(|health| {
        health
            .sources
            .get(&phoxal_cli_observation::ObservationSource::Input)
    }) != Some(&phoxal_cli_observation::SourceStatus::Failed);
    match key.code {
        Key::Up => move_device_candidate(&mut model.input, &devices, -1),
        Key::Down => move_device_candidate(&mut model.input, &devices, 1),
        Key::Enter => {
            if input_fresh && let Some(candidate) = model.input.candidate.clone() {
                model.input.pending_selection = Some(candidate.clone());
                return vec![Effect::InputSelect(candidate)];
            }
        }
        Key::Char('e') if input_fresh => {
            model.input.pending_enabled = Some(true);
            return vec![Effect::InputEnable(true)];
        }
        Key::Char('x') if input_fresh => {
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
    devices: &[phoxal_cli_observation::JoypadDevice],
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
    match &mut model.route {
        FocusRoute::Modal { modal, .. } if *modal == id => close_modal(model),
        FocusRoute::Modal { modal, .. } => *modal = id,
        _ => {
            let route = std::mem::take(&mut model.route);
            model.route = route.open_modal(id);
        }
    }
}

fn close_modal(model: &mut AppModel) {
    if let FocusRoute::Modal { return_to, .. } = std::mem::take(&mut model.route) {
        model.route = *return_to;
    }
}

fn handle_modal_key(model: &mut AppModel, key: KeyEvent) -> Vec<Effect> {
    let modal = match model.route {
        FocusRoute::Modal { modal, .. } => modal,
        _ => return Vec::new(),
    };
    match key.code {
        Key::Char('?') => {
            open_modal(model, ModalId::Help);
            Vec::new()
        }
        Key::Char('i') => {
            open_modal(model, ModalId::SessionInfo);
            Vec::new()
        }
        Key::Esc => {
            close_modal(model);
            Vec::new()
        }
        Key::Enter if modal == ModalId::ConfirmStop => request_stop(model),
        // `q` is always detach, including out of the confirmation: a modal an
        // operator opened by reflex must have an exit that does nothing to the
        // robot.
        Key::Char('q') if modal == ModalId::ConfirmStop => {
            close_modal(model);
            detach(model)
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Instant;

    use phoxal_cli_observation::{
        AttachmentEpoch, AttachmentEvent, InputObservation, JoypadDevice, JoypadDeviceStatus,
        JoypadDevicesSample, LogSeverity, LogSource, LogWindow, ObservationWindow,
        ProcessObservation, SupervisorObservation,
    };
    use phoxal_client::supervisor::execution::{
        DesiredState, Detail, Process, ProcessState, SupervisorFailure, SupervisorFailureReason,
    };
    use phoxal_runtime_contract::clock::Clock;
    use phoxal_runtime_contract::identity::ExecutionId;
    use phoxal_runtime_contract::identity::RobotId;
    use phoxal_runtime_contract::identity::{ParticipantId, ProducerId};

    use super::*;

    fn epoch() -> AttachmentEpoch {
        AttachmentEpoch::new(
            ExecutionId::parse(&"1".repeat(ExecutionId::LEN)).expect("fixed execution id"),
        )
    }

    #[test]
    fn one_thousand_log_invalidations_coalesce_while_read_is_in_flight() {
        let mut model = AppModel {
            epoch: Some(epoch()),
            ..AppModel::default()
        };
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
        assert_eq!(first.body.direction, WindowDirection::Forward);

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
        assert_eq!(model.logs.known_revision, StoreRevision(1));
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
                epoch: AttachmentEpoch::new(ExecutionId::mint()),
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
            let recovery = update(&mut model, Msg::Logs(LogsMsg::Window(window)));
            assert_eq!(recovery.len(), 1);
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
    fn pausing_an_empty_log_view_anchors_before_future_lines() {
        let mut model = AppModel {
            route: FocusRoute::Content {
                panel: PanelId::Logs(LogsPanelId::Stream),
            },
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char(' ').into())),
        );
        assert!(!model.logs.follow);
        assert!(model.logs.pause_anchor.is_some());
    }

    /// Every row in the snapshot is a supervised participant, so the split is
    /// "came from the graph" versus "came from the supervisor or this client".
    #[test]
    fn log_source_filter_separates_supervised_participants_from_everything_else() {
        let runtime_key = ParticipantId::new("drive").expect("fixture participant");
        let processes = BTreeMap::from([(runtime_key.clone(), process(runtime_key))]);
        let row = |participant: &str| LogRow {
            participant: participant.to_string(),
            source: LogSource::Raw,
            severity: LogSeverity::Info,
            text: String::new(),
            event_time: std::time::SystemTime::UNIX_EPOCH,
        };

        assert!(log_matches_source(
            LogSourceFilter::Runtimes,
            &row("drive"),
            &processes
        ));
        assert!(!log_matches_source(
            LogSourceFilter::System,
            &row("drive"),
            &processes
        ));
        assert!(log_matches_source(
            LogSourceFilter::System,
            &row("log"),
            &processes
        ));
        assert!(
            log_matches_source(LogSourceFilter::System, &row("phoxal-cli"), &processes),
            "unmatched local diagnostics belong to the system stream"
        );
        assert!(
            log_matches_source(LogSourceFilter::System, &row("supervisor"), &processes),
            "the supervisor's own records are never graph records"
        );
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
            Msg::Navigate(NavigationMsg::Key(Key::Char('?').into())),
        );
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Esc.into())),
        );
        assert_eq!(model.route, return_to);
    }

    #[test]
    fn help_and_session_shortcuts_toggle_or_replace_the_active_modal() {
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('?').into())),
        );
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('i').into())),
        );
        assert!(matches!(
            model.route,
            FocusRoute::Modal {
                modal: ModalId::SessionInfo,
                ..
            }
        ));
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('i').into())),
        );
        assert!(!matches!(model.route, FocusRoute::Modal { .. }));
    }

    fn control_c() -> Msg {
        Msg::Navigate(NavigationMsg::Key(KeyEvent::new(
            Key::Char('c'),
            KeyModifiers::CONTROL,
        )))
    }

    /// `q` detaches and leaves the supervisor running; Ctrl+C takes two presses to
    /// stop, and the session keeps rendering until the supervisor's own
    /// terminal snapshot arrives.
    #[test]
    fn q_detaches_while_stopping_takes_two_interrupts_and_then_waits() {
        let mut model = AppModel::default();
        let detach = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('q').into())),
        );
        assert!(detach.is_empty());
        assert_eq!(model.exit, Some(AttachmentOutcome::Detached));

        let mut model = AppModel::default();
        let first = update(&mut model, control_c());
        assert!(first.is_empty(), "the first interrupt sends nothing");
        assert_eq!(model.route.modal(), Some(ModalId::ConfirmStop));

        let second = update(&mut model, control_c());
        assert_eq!(second, vec![Effect::StopProject]);
        assert!(model.stop_requested);
        assert_eq!(
            model.exit, None,
            "the session waits for the supervisor's terminal snapshot"
        );

        // A third interrupt does not send a second stop.
        assert!(update(&mut model, control_c()).is_empty());
    }

    /// A modal opened by reflex must have an exit that does nothing to the
    /// robot, and Esc must simply cancel.
    #[test]
    fn q_in_the_confirmation_detaches_and_escape_cancels() {
        let mut model = AppModel::default();
        update(&mut model, control_c());
        let detach = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('q').into())),
        );
        assert!(detach.is_empty());
        assert_eq!(model.exit, Some(AttachmentOutcome::Detached));

        let mut model = AppModel::default();
        update(&mut model, control_c());
        let cancelled = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Esc.into())),
        );
        assert!(cancelled.is_empty());
        assert_eq!(model.route.modal(), None);
        assert_eq!(model.exit, None);
        assert!(!model.stop_requested);
    }

    /// A simulation session is not detachable: the client owns Webots, so `q`
    /// ends the whole session rather than stranding a simulator.
    #[test]
    fn q_in_a_simulation_session_stops_instead_of_detaching() {
        let mut model = AppModel {
            detachable: false,
            ..AppModel::default()
        };
        let stop = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('q').into())),
        );
        assert_eq!(stop, vec![Effect::StopProject]);
        assert_eq!(model.exit, None);
        assert!(model.stop_requested);
    }

    #[test]
    fn failed_execution_is_distinct_from_a_clean_stop() {
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Client(AttachmentEvent::SupervisorChanged(Arc::new(supervisor(
                Lifecycle::Failed,
            )))),
        );
        assert_eq!(
            model.exit,
            Some(AttachmentOutcome::ExecutionFailed { reason: None })
        );
    }

    #[test]
    fn supervisor_failure_reason_flows_into_the_exit_outcome() {
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Client(AttachmentEvent::SupervisorChanged(Arc::new(
                supervisor_with_failure(
                    Lifecycle::Failed,
                    Some("catalog train floor not supported: 0.41.2 < 0.42.0"),
                ),
            ))),
        );
        assert_eq!(
            model.exit,
            model.exit.clone().filter(|exit| matches!(
                exit,
                AttachmentOutcome::ExecutionFailed { reason: Some(failure) }
                    if failure.detail.as_str()
                        == "catalog train floor not supported: 0.41.2 < 0.42.0"
            ))
        );
    }

    #[test]
    fn a_supervisor_failure_reason_wins_over_a_connection_loss_in_either_ordering() {
        for lost_first in [false, true] {
            let mut model = AppModel::default();
            let lost = Msg::Client(AttachmentEvent::ConnectionChanged(
                phoxal_cli_observation::ConnectionObservation::Lost {
                    reason: "stream closed".into(),
                },
            ));
            let failed = Msg::Client(AttachmentEvent::SupervisorChanged(Arc::new(
                supervisor_with_failure(Lifecycle::Failed, Some("prepare failed")),
            )));
            if lost_first {
                update(&mut model, lost);
                update(&mut model, failed);
            } else {
                update(&mut model, failed);
                update(&mut model, lost);
            }
            assert!(
                matches!(
                    &model.exit,
                    Some(AttachmentOutcome::ExecutionFailed { reason: Some(failure) })
                        if failure.detail.as_str() == "prepare failed"
                ),
                "lost_first={lost_first}: {:?}",
                model.exit
            );
        }
    }

    #[test]
    fn permanently_lost_execution_is_a_failed_outcome_with_no_supervisor_reason() {
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Client(AttachmentEvent::ConnectionChanged(
                phoxal_cli_observation::ConnectionObservation::Lost {
                    reason: "protocol mismatch".into(),
                },
            )),
        );
        // The transport-level reason ("protocol mismatch") is deliberately
        // NOT carried into the outcome: a connection loss is never a
        // supervisor-reported cause, so the caller must fall through to its
        // own supervisor.log pointer instead.
        assert_eq!(
            model.exit,
            Some(AttachmentOutcome::ExecutionFailed { reason: None })
        );
    }

    #[test]
    fn terminal_stop_wins_every_connection_closure_ordering() {
        for lost_first in [false, true] {
            for _ in 0..50 {
                let mut model = AppModel::default();
                let lost = Msg::Client(AttachmentEvent::ConnectionChanged(
                    phoxal_cli_observation::ConnectionObservation::Lost {
                        reason: "stream closed".into(),
                    },
                ));
                let stopped = Msg::Client(AttachmentEvent::SupervisorChanged(Arc::new(
                    supervisor(Lifecycle::Stopped),
                )));
                if lost_first {
                    update(&mut model, lost);
                    update(&mut model, stopped);
                } else {
                    update(&mut model, stopped);
                    update(&mut model, lost);
                }
                assert_eq!(model.exit, Some(AttachmentOutcome::ExecutionStopped));
            }
        }
    }

    #[test]
    fn runtime_candidate_tracks_identity_and_never_retargets_a_removed_row() {
        let alpha = ParticipantId::new("alpha").expect("fixture participant");
        let beta = ParticipantId::new("beta").expect("fixture participant");
        let mut processes = BTreeMap::from([
            (alpha.clone(), process(alpha.clone())),
            (beta.clone(), process(beta.clone())),
        ]);
        let mut model = AppModel {
            epoch: Some(epoch()),
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Client(AttachmentEvent::ProcessesChanged {
                epoch: epoch(),
                values: Arc::new(processes.clone()),
            }),
        );
        model.runtimes.candidate = Some(beta.clone());
        update(
            &mut model,
            Msg::Client(AttachmentEvent::ProcessesChanged {
                epoch: epoch(),
                values: Arc::new(processes.clone()),
            }),
        );
        assert_eq!(model.runtimes.candidate, Some(beta.clone()));

        processes.remove(&beta);
        update(
            &mut model,
            Msg::Client(AttachmentEvent::ProcessesChanged {
                epoch: epoch(),
                values: Arc::new(processes),
            }),
        );
        assert_eq!(model.runtimes.candidate, Some(alpha));
    }

    /// A process the snapshot no longer carries cannot stay selected: the
    /// detail view is cleared and the widened read is re-issued.
    #[test]
    fn runtime_detail_clears_and_requeries_when_the_process_leaves_the_snapshot() {
        let key = ParticipantId::new("drive").expect("fixture participant");
        let mut model = AppModel {
            epoch: Some(epoch()),
            ..AppModel::default()
        };
        model.runtimes.candidate = Some(key.clone());
        model.runtimes.detail = Some(key.clone());
        let processes = Arc::new(BTreeMap::new());
        let effects = update(
            &mut model,
            Msg::Client(AttachmentEvent::ProcessesChanged {
                epoch: epoch(),
                values: processes,
            }),
        );
        assert_eq!(model.runtimes.detail, None);
        let Effect::ReadRuntimes(read) = &effects[0] else {
            panic!("expected widened runtime read");
        };
        assert_eq!(read.body.participant, None);
    }

    #[test]
    fn restart_without_a_live_producer_is_a_visible_diagnostic() {
        let key = ParticipantId::new("drive").expect("fixture participant");
        let mut process = process(key.clone());
        process.row.producer = None;
        let mut model = AppModel {
            epoch: Some(epoch()),
            route: FocusRoute::Content {
                panel: PanelId::Runtimes(RuntimesPanelId::Processes),
            },
            ..AppModel::default()
        };
        model.overview.processes = Arc::new(BTreeMap::from([(key.clone(), process)]));
        model.runtimes.candidate = Some(key);
        let effects = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('r').into())),
        );
        assert!(effects.is_empty());
        assert!(model.overview.diagnostics[0].contains("no restartable producer"));
    }

    #[test]
    fn runtime_log_jump_resets_every_stale_filter_and_pause() {
        let key = ParticipantId::new("drive").expect("fixture participant");
        let mut model = AppModel {
            route: FocusRoute::Content {
                panel: PanelId::Runtimes(RuntimesPanelId::Processes),
            },
            ..AppModel::default()
        };
        model.runtimes.candidate = Some(key);
        model.logs.text = "old".to_string();
        model.logs.severity = crate::components::logs::SeverityFilter::Error;
        model.logs.follow = false;
        model.logs.pause_anchor = Some(SystemTime::UNIX_EPOCH);
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('l').into())),
        );
        assert!(model.logs.text.is_empty());
        assert_eq!(
            model.logs.severity,
            crate::components::logs::SeverityFilter::All
        );
        assert!(model.logs.follow);
        assert_eq!(model.logs.pause_anchor, None);
    }

    #[test]
    /// A telemetry record is stamped with the participant id, and a driver's
    /// participant id is its component instance - so the query carries the
    /// instance, not the rendered key.
    fn runtime_detail_queries_use_the_participant_id_and_clear_on_escape() {
        let key = ParticipantId::new("base").expect("fixture participant");
        let mut model = AppModel {
            epoch: Some(epoch()),
            route: FocusRoute::Content {
                panel: PanelId::Runtimes(RuntimesPanelId::Processes),
            },
            ..AppModel::default()
        };
        model.runtimes.candidate = Some(key.clone());
        let effects = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Enter.into())),
        );
        let Effect::ReadRuntimes(read) = &effects[0] else {
            panic!("expected runtime read");
        };
        assert_eq!(read.body.participant.as_deref(), Some("base"));
        assert_eq!(model.runtimes.detail, Some(key));
        model.runtimes.in_flight = None;
        let effects = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Esc.into())),
        );
        assert_eq!(model.runtimes.detail, None);
        let Effect::ReadRuntimes(read) = &effects[0] else {
            panic!("expected widened runtime read");
        };
        assert_eq!(read.body.participant, None);
    }

    #[test]
    fn input_candidate_is_distinct_from_authoritative_selection_and_acknowledgement() {
        let mut model = AppModel {
            epoch: Some(epoch()),
            route: FocusRoute::Content {
                panel: PanelId::Input(InputPanelId::Devices),
            },
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Client(AttachmentEvent::InputChanged {
                epoch: epoch(),
                values: Arc::new(input_observation(Some("pad-a"), true)),
            }),
        );
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Down.into())),
        );
        assert_eq!(model.input.candidate, Some(DeviceId("pad-b".to_string())));
        assert_eq!(
            model
                .input
                .observation
                .as_ref()
                .and_then(|input| input.joypads.selected.as_deref()),
            Some("pad-a")
        );

        let effects = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Enter.into())),
        );
        assert_eq!(
            effects,
            vec![Effect::InputSelect(DeviceId("pad-b".to_string()))]
        );
        assert_eq!(
            model.input.pending_selection,
            Some(DeviceId("pad-b".to_string()))
        );

        update(
            &mut model,
            Msg::Client(AttachmentEvent::InputChanged {
                epoch: epoch(),
                values: Arc::new(input_observation(Some("pad-b"), true)),
            }),
        );
        assert_eq!(model.input.pending_selection, None);
    }

    #[test]
    fn stale_input_cannot_be_selected() {
        let mut model = AppModel {
            route: FocusRoute::Content {
                panel: PanelId::Input(InputPanelId::Devices),
            },
            ..AppModel::default()
        };
        let mut health = phoxal_cli_observation::SourceHealth::default();
        health.sources.insert(
            phoxal_cli_observation::ObservationSource::Input,
            phoxal_cli_observation::SourceStatus::Failed,
        );
        model.overview.source_health = Some(Arc::new(health));
        model
            .input
            .reconcile_authoritative(input_observation(Some("pad-a"), true));
        let effects = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Enter.into())),
        );
        assert!(effects.is_empty());
        assert_eq!(model.input.pending_selection, None);
    }

    #[test]
    fn tab_candidate_does_not_switch_page_before_enter() {
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Right.into())),
        );
        assert_eq!(model.route.page(), PageId::Overview);
        assert!(matches!(
            model.route,
            FocusRoute::Tabs {
                page: PageId::Overview,
                candidate: PageId::Runtimes
            }
        ));
    }

    #[test]
    fn resize_and_focus_refresh_request_a_full_clear() {
        let mut model = AppModel {
            clear_requested: false,
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Refresh { clear: true }),
        );
        assert!(model.clear_requested);
    }

    fn supervisor(lifecycle: Lifecycle) -> SupervisorObservation {
        supervisor_with_failure(lifecycle, None)
    }

    fn supervisor_with_failure(
        lifecycle: Lifecycle,
        failure: Option<&str>,
    ) -> SupervisorObservation {
        SupervisorObservation {
            revision: 1,
            execution: epoch().execution,
            robot: RobotId::new("testbot").expect("fixture robot id"),
            clock: Clock::Real,
            project: "/tmp/robot".to_string(),
            lifecycle,
            startup: Vec::new(),
            failure: failure.map(|detail| SupervisorFailure {
                reason: SupervisorFailureReason::LaunchFailed,
                detail: Detail::new(detail),
            }),
        }
    }

    fn input_observation(selected: Option<&str>, enabled: bool) -> InputObservation {
        InputObservation {
            joypads: JoypadDevicesSample {
                available: Arc::new(vec![
                    JoypadDevice {
                        id: "pad-a".to_string(),
                        name: "A".to_string(),
                        status: JoypadDeviceStatus::Ready,
                    },
                    JoypadDevice {
                        id: "pad-b".to_string(),
                        name: "B".to_string(),
                        status: JoypadDeviceStatus::Ready,
                    },
                ]),
                selected: selected.map(str::to_string),
                enabled,
                ..JoypadDevicesSample::default()
            },
            motion: None,
        }
    }

    fn process(participant: ParticipantId) -> ProcessObservation {
        ProcessObservation {
            row: Process {
                participant,
                kind: phoxal_runtime_contract::metadata::ParticipantKind::Service,
                component: None,
                desired: DesiredState::Running,
                state: ProcessState::Ready,
                pid: Some(42),
                producer: Some(
                    ProducerId::try_from((1_u128 << 124) | 43).expect("fixture producer"),
                ),
                restarts: 0,
                failure: None,
            },
            observed_started_at: Instant::now(),
            observed_ended_at: None,
            observed_first_ready_at: Some(Instant::now()),
        }
    }
}
