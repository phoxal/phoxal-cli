//! Pure Elm-style update function.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::SystemTime;

use phoxal_cli_core::session::{ParticipantKind, ProcessScope, ProjectLifecycle, RobotScope};
use phoxal_cli_observation::{
    AttachmentEvent, BusQuery, BusRead, BusRow, LogAnchor, LogFilters, LogQuery, LogRead, LogRow,
    ProcessObservation, ProcessTable, QueryToken, RuntimeQuery, RuntimeRead, RuntimeRow,
    StoreChanged, StoreRevision, WindowDirection,
};
use tuirealm::event::{Key, KeyEvent, KeyModifiers};

use crate::components::bus::BusSort;
use crate::components::input::InputModel;
use crate::components::logs::{Editing, LogSourceFilter};

use super::effect::{AttachmentOutcome, DeviceId, Effect};
use super::id::{BusPanelId, InputPanelId, LogsPanelId, ModalId, PageId, PanelId, RuntimesPanelId};
use super::message::{BusMsg, LogsMsg, Msg, NavigationMsg, RuntimesMsg};
use super::model::AppModel;
use super::route::{FocusRoute, cycle_panel};

const LOG_WINDOW_LIMIT: usize = 2_000;
const BUS_WINDOW_LIMIT: usize = 16_384;
const RUNTIME_WINDOW_LIMIT: usize = 4_096;

pub fn update(model: &mut AppModel, message: Msg) -> Vec<Effect> {
    let wake = matches!(message, Msg::Wake);
    let effects = match message {
        Msg::Wake => Vec::new(),
        Msg::Terminate => detach(model),
        Msg::Diagnostic(message) => {
            model.overview.push_diagnostic(message);
            Vec::new()
        }
        Msg::Client(event) => update_client(model, event),
        Msg::Navigate(message) => update_navigation(model, message),
        Msg::Logs(LogsMsg::Window(window)) => accept_logs(model, window),
        Msg::Bus(BusMsg::Window(window)) => accept_bus(model, window),
        Msg::Runtimes(RuntimesMsg::Window(window)) => accept_runtimes(model, window),
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
            model.exit = match supervisor.lifecycle {
                ProjectLifecycle::Stopped => Some(AttachmentOutcome::ResidentStopped),
                ProjectLifecycle::Failed => Some(AttachmentOutcome::ResidentFailed),
                _ => model.exit,
            };
            model.overview.supervisor = Some(supervisor);
            Vec::new()
        }
        AttachmentEvent::ProcessesChanged(processes) => {
            let detail_cleared = preserve_process_candidate(model, &processes);
            model.overview.processes = processes;
            if detail_cleared {
                refresh_runtimes(model)
            } else {
                Vec::new()
            }
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
) -> bool {
    if model
        .runtimes
        .candidate
        .as_ref()
        .is_none_or(|candidate| !processes.get(candidate).is_some_and(process_is_runtime))
    {
        model.runtimes.candidate = processes
            .iter()
            .find(|(_, process)| process_is_runtime(process))
            .map(|(key, _)| key.clone());
    }
    let detail_cleared = model
        .runtimes
        .detail
        .as_ref()
        .is_some_and(|detail| !processes.get(detail).is_some_and(process_is_runtime));
    if detail_cleared {
        model.runtimes.detail = None;
    }
    detail_cleared
}

pub(crate) fn process_is_runtime(process: &ProcessObservation) -> bool {
    matches!(
        process.kind,
        ParticipantKind::Service | ParticipantKind::Driver
    ) && (process.user_service || !is_known_internal_id(&process.key.id))
}

fn participant_is_internal(participant: &str, processes: &ProcessTable) -> bool {
    processes
        .iter()
        .find(|(key, _)| key.id == participant || key.to_string() == participant)
        .map_or_else(
            || is_known_internal_id(participant),
            |(_, process)| !process_is_runtime(process),
        )
}

fn is_known_internal_id(id: &str) -> bool {
    id.eq_ignore_ascii_case("phoxal-cli")
        || starts_with_ignore_ascii_case(id, "phoxal-cli/")
        || starts_with_ignore_ascii_case(id, "tool-")
        || id.eq_ignore_ascii_case("supervisor")
        || id.eq_ignore_ascii_case(phoxal_cli_core::session::WEBOTS_PROCESS_ID)
        || starts_with_ignore_ascii_case(id, "webots-")
        || starts_with_ignore_ascii_case(id, "simulator-")
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
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
    let runtime = processes.iter().any(|(key, process)| {
        (key.id == row.participant || key.to_string() == row.participant)
            && process_is_runtime(process)
    });
    match source {
        LogSourceFilter::All => true,
        LogSourceFilter::Runtimes => runtime,
        LogSourceFilter::Tools => !runtime,
    }
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
        model.bus.in_flight = None;
        model.bus.dirty_revision = Some(newest(model.bus.dirty_revision, observed_revision));
        return issue_bus_if_idle(model).into_iter().collect();
    }
    let mut newest_rows = BTreeMap::<(RobotScope, String, String), BusRow>::new();
    for row in window.rows.iter().filter(|row| {
        (row.aggregate_overflow || !row.topic.is_empty())
            && (model.bus.show_internal
                || row.aggregate_overflow
                || !participant_is_internal(&row.participant, &model.overview.processes))
    }) {
        let key = (
            row.scope.clone(),
            row.topic.clone(),
            row.participant.clone(),
        );
        if newest_rows
            .get(&key)
            .is_none_or(|current| row.observed_at >= current.observed_at)
        {
            newest_rows.insert(key, row.clone());
        }
    }
    model.bus.rows = newest_rows.into_values().collect();
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
            participant: model.runtimes.detail.as_ref().map(|key| key.id.clone()),
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
    let detail_scope = model
        .runtimes
        .detail
        .as_ref()
        .and_then(|key| match &key.scope {
            ProcessScope::Robot(robot) => Some(robot),
            ProcessScope::Project => None,
        });
    let mut newest_rows = BTreeMap::<(RobotScope, String), RuntimeRow>::new();
    for row in window.rows.iter().filter(|row| {
        detail_scope.is_none_or(|scope| {
            row.scope.namespace == scope.namespace && row.scope.robot_id == scope.robot_id
        })
    }) {
        let key = (row.scope.clone(), row.sample.participant_id.clone());
        newest_rows.insert(key, row.clone());
    }
    model.runtimes.rows = newest_rows.into_values().collect();
    model.runtimes.known_revision = window.revision;
    model.runtimes.in_flight = None;
    issue_runtimes_if_idle(model).into_iter().collect()
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
        '4' => Some(PageId::Bus),
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
        PanelId::Bus(panel) => handle_bus_key(model, panel, key),
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
    let keys: Vec<_> = model
        .overview
        .processes
        .iter()
        .filter(|(_, process)| process_is_runtime(process))
        .map(|(key, _)| key.clone())
        .collect();
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
        .entry
        .status
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
            Key::Char('a') => {
                model.bus.show_internal = !model.bus.show_internal;
                return refresh_bus(model);
            }
            Key::Enter => match model.bus.control_candidate {
                0 => model.bus.editing = true,
                1 => {
                    model.bus.sort = model.bus.sort.cycle();
                    sort_bus(model);
                }
                2 => {
                    model.bus.show_internal = !model.bus.show_internal;
                    return refresh_bus(model);
                }
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
    let input_fresh = model
        .overview
        .freshness
        .get("input")
        .is_none_or(|freshness| *freshness == phoxal_cli_observation::Freshness::Fresh);
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
        Key::Enter if modal == ModalId::ConfirmStop => {
            close_modal(model);
            vec![Effect::StopProject]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Instant;

    use phoxal_cli_core::identity::ExecutionId;
    use phoxal_cli_core::session::{
        JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample, LogSeverity, LogSource,
        ParticipantKind, ParticipantState, ProcessDescriptor, ProcessEntry, ProcessKey,
        ProcessStatus, RobotKey, RobotScope, RuntimePerformanceSample, StartupRequirement,
        StartupStatus,
    };
    use phoxal_cli_observation::{
        AttachmentEpoch, AttachmentEvent, BusRow, BusWindow, Freshness, InputObservation,
        LogWindow, ObservationWindow, ProcessObservation, RuntimeRow, RuntimeWindow,
        SupervisorObservation,
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

    #[test]
    fn log_source_filter_separates_runtime_participants_from_tools() {
        let runtime_key = ProcessKey::project("drive");
        let tool_key = ProcessKey::project("infrastructure-router");
        let processes = BTreeMap::from([
            (
                runtime_key.clone(),
                process_with_kind(runtime_key, ParticipantKind::Service),
            ),
            (
                tool_key.clone(),
                process_with_kind(tool_key, ParticipantKind::Tool),
            ),
        ]);
        let row = |participant: &str| LogRow {
            participant: participant.to_string(),
            source: LogSource::Raw,
            severity: LogSeverity::Info,
            text: String::new(),
            event_time: std::time::SystemTime::UNIX_EPOCH,
            scope: None,
        };

        assert!(log_matches_source(
            LogSourceFilter::Runtimes,
            &row("drive"),
            &processes
        ));
        assert!(!log_matches_source(
            LogSourceFilter::Tools,
            &row("drive"),
            &processes
        ));
        assert!(log_matches_source(
            LogSourceFilter::Tools,
            &row("infrastructure-router"),
            &processes
        ));
        assert!(
            log_matches_source(LogSourceFilter::Tools, &row("phoxal-cli"), &processes),
            "unmatched local diagnostics belong to the tools stream"
        );
        let internal_key = ProcessKey::project("supervisor");
        let mut internal = process_with_kind(internal_key.clone(), ParticipantKind::Service);
        internal.user_service = false;
        let processes = BTreeMap::from([(internal_key, internal)]);
        assert!(log_matches_source(
            LogSourceFilter::Tools,
            &row("supervisor"),
            &processes
        ));
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

    #[test]
    fn stop_requires_confirmation_while_q_only_detaches() {
        let mut model = AppModel::default();
        let detach = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('q').into())),
        );
        assert_eq!(detach, vec![Effect::Detach]);
        assert_eq!(model.exit, Some(AttachmentOutcome::Detached));

        let mut model = AppModel::default();
        let open = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('S').into())),
        );
        assert!(open.is_empty());
        assert!(matches!(
            model.route,
            FocusRoute::Modal {
                modal: ModalId::ConfirmStop,
                ..
            }
        ));
        let stop = update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Enter.into())),
        );
        assert_eq!(stop, vec![Effect::StopProject]);
        assert_eq!(model.exit, None);
    }

    #[test]
    fn failed_resident_is_distinct_from_a_clean_stop() {
        let mut model = AppModel::default();
        update(
            &mut model,
            Msg::Client(AttachmentEvent::SupervisorChanged(Arc::new(supervisor(
                ProjectLifecycle::Failed,
            )))),
        );
        assert_eq!(model.exit, Some(AttachmentOutcome::ResidentFailed));
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

    #[test]
    fn runtime_detail_clears_and_requeries_when_process_stops_qualifying() {
        let key = ProcessKey::project("drive");
        let mut model = AppModel {
            epoch: Some(epoch()),
            ..AppModel::default()
        };
        model.runtimes.candidate = Some(key.clone());
        model.runtimes.detail = Some(key.clone());
        let processes = Arc::new(BTreeMap::from([(
            key.clone(),
            process_with_kind(key, ParticipantKind::Tool),
        )]));
        let effects = update(
            &mut model,
            Msg::Client(AttachmentEvent::ProcessesChanged(processes)),
        );
        assert_eq!(model.runtimes.detail, None);
        let Effect::ReadRuntimes(read) = &effects[0] else {
            panic!("expected widened runtime read");
        };
        assert_eq!(read.body.participant, None);
    }

    #[test]
    fn restart_without_a_live_producer_is_a_visible_diagnostic() {
        let key = ProcessKey::project("drive");
        let mut process = process(key.clone());
        process.entry.status.producer = None;
        let mut model = AppModel {
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
        let key = ProcessKey::project("drive");
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
    fn bus_and_runtime_windows_keep_only_the_latest_scoped_row() {
        let scope = RobotScope {
            namespace: "lab".to_string(),
            robot_id: "rover".to_string(),
        };
        let mut model = AppModel {
            epoch: Some(epoch()),
            ..AppModel::default()
        };
        model.bus.in_flight = Some((epoch(), StoreRevision(2), QueryToken(1)));
        let now = Instant::now();
        update(
            &mut model,
            Msg::Bus(BusMsg::Window(BusWindow {
                epoch: epoch(),
                revision: StoreRevision(2),
                token: QueryToken(1),
                rows: Arc::from([
                    bus_row(&scope, now, "drive", "v1/state", 1),
                    bus_row(
                        &scope,
                        now + std::time::Duration::from_secs(1),
                        "drive",
                        "v1/state",
                        2,
                    ),
                ]),
            })),
        );
        assert_eq!(model.bus.rows.len(), 1);
        assert_eq!(model.bus.rows[0].count, 2);

        model.bus.in_flight = Some((epoch(), StoreRevision(3), QueryToken(3)));
        let mut overflow = bus_row(
            &scope,
            now + std::time::Duration::from_secs(2),
            "supervisor",
            "",
            3,
        );
        overflow.aggregate_overflow = true;
        update(
            &mut model,
            Msg::Bus(BusMsg::Window(BusWindow {
                epoch: epoch(),
                revision: StoreRevision(3),
                token: QueryToken(3),
                rows: Arc::from([overflow]),
            })),
        );
        assert_eq!(model.bus.rows.len(), 1);
        assert!(model.bus.rows[0].aggregate_overflow);

        model.runtimes.in_flight = Some((epoch(), StoreRevision(2), QueryToken(2)));
        update(
            &mut model,
            Msg::Runtimes(RuntimesMsg::Window(RuntimeWindow {
                epoch: epoch(),
                revision: StoreRevision(2),
                token: QueryToken(2),
                rows: Arc::from([
                    runtime_row(&scope, "drive", 9),
                    runtime_row(&scope, "drive", 1),
                ]),
            })),
        );
        assert_eq!(model.runtimes.rows.len(), 1);
        assert_eq!(model.runtimes.rows[0].sample.sequence, 1);
    }

    #[test]
    fn runtime_detail_queries_use_bare_id_and_clear_on_escape() {
        let key = ProcessKey::robot(RobotKey::new("lab", "rover"), "drive");
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
        assert_eq!(read.body.participant.as_deref(), Some("drive"));
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
    fn bus_controls_are_local_and_filter_editing_consumes_keys() {
        let mut model = AppModel {
            route: FocusRoute::Content {
                panel: PanelId::Bus(BusPanelId::Controls),
            },
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('/').into())),
        );
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('s').into())),
        );
        assert_eq!(model.bus.filter, "s");
        assert_eq!(model.bus.sort, BusSort::Rate);
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Enter.into())),
        );
        update(
            &mut model,
            Msg::Navigate(NavigationMsg::Key(Key::Char('s').into())),
        );
        assert_eq!(model.bus.sort, BusSort::Topic);
        assert_eq!(model.logs.text, "");
    }

    #[test]
    fn input_candidate_is_distinct_from_authoritative_selection_and_acknowledgement() {
        let mut model = AppModel {
            route: FocusRoute::Content {
                panel: PanelId::Input(InputPanelId::Devices),
            },
            ..AppModel::default()
        };
        update(
            &mut model,
            Msg::Client(AttachmentEvent::InputChanged(Arc::new(input_observation(
                Some("pad-a"),
                true,
            )))),
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
            Msg::Client(AttachmentEvent::InputChanged(Arc::new(input_observation(
                Some("pad-b"),
                true,
            )))),
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
        model
            .overview
            .freshness
            .insert("input".to_string(), Freshness::Stale);
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

    fn bus_row(
        scope: &RobotScope,
        observed_at: Instant,
        participant: &str,
        topic: &str,
        count: u64,
    ) -> BusRow {
        BusRow {
            scope: scope.clone(),
            observed_at,
            topic: topic.to_string(),
            participant: participant.to_string(),
            rate_hz: count as f32,
            count,
            aggregate_overflow: false,
            topics_truncated: 0,
            throughput_msg_s: count as f32,
            window_ns: 1,
        }
    }

    fn runtime_row(scope: &RobotScope, participant: &str, sequence: u64) -> RuntimeRow {
        RuntimeRow {
            scope: scope.clone(),
            sample: RuntimePerformanceSample {
                sequence,
                participant_id: participant.to_string(),
                truncated: 0,
                window_ns: 1,
                step: None,
                topics: Arc::new(Vec::new()),
                overflow: None,
            },
            capacity_evictions: 0,
        }
    }

    fn supervisor(lifecycle: ProjectLifecycle) -> SupervisorObservation {
        SupervisorObservation {
            supervisor_generation: 1,
            revision: 1,
            execution_id: epoch().execution_id,
            project: "/tmp/robot".to_string(),
            entry: "/tmp/robot/robot.yaml".to_string(),
            framework_train: "0.43.2".to_string(),
            simulation: None,
            lifecycle,
            router: "unixsock-stream/test".to_string(),
            plan_revision: 1,
            graph_generation: 1,
            startup: StartupStatus {
                completed_phases: Vec::new(),
                active_phase: None,
            },
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

    fn process(key: ProcessKey) -> ProcessObservation {
        process_with_kind(key, ParticipantKind::Service)
    }

    fn process_with_kind(key: ProcessKey, kind: ParticipantKind) -> ProcessObservation {
        ProcessObservation {
            key: key.clone(),
            entry: ProcessEntry {
                descriptor: ProcessDescriptor {
                    key,
                    kind,
                    artifact: "service".to_string(),
                    owner: "test".to_string(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus::default(),
            },
            kind,
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
