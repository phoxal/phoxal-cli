//! Ratatui rendering for the fixed Overview, Runtimes, Logs, Bus, and Input pages.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use crate::ColorCapability;
use crate::ratatui as color;
use crate::{Role, Theme};
use phoxal_cli_core::session::human;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
    Sparkline, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::startup::{PhaseRow, StartupState};
#[cfg(test)]
use crate::tui::state::LogSourceFilter;
use crate::tui::state::{AppState, BusSort, CaseInsensitiveNeedle, NavigationLevel, Page};
use crate::tui::view_model::{RuntimeGroup, SessionViewModel};
#[cfg(test)]
use phoxal_cli_core::project::launch_plan::ROBOT_TOOL_JOYPAD;
#[cfg(test)]
use phoxal_cli_core::session::JoypadDevicesSample;
use phoxal_cli_core::session::SessionMode;
use phoxal_cli_core::session::event::PhaseOutcome;
use phoxal_cli_core::session::stores::log::sanitize_terminal_text;
use phoxal_cli_core::session::stores::telemetry::{DEFAULT_FRESHNESS_TTL, Timestamped};
use phoxal_cli_core::session::{ClockSample, LogSeverity, ParticipantState, ParticipantStatus};
use phoxal_cli_core::session::{
    DeviceSample, JoypadDeviceStatus, RuntimeBufferKind, RuntimeDirection,
    RuntimePerformanceSample, TelemetrySnapshot, TopicMetric,
};

#[derive(Debug, Clone)]
pub struct TitleInfo {
    pub robot: String,
    pub namespace: String,
    pub train: String,
    pub manifest: String,
    pub mode: SessionMode,
    pub bus_endpoint: String,
    pub started_at: SystemTime,
    pub started_instant: Instant,
}

fn state_role(state: ParticipantState) -> Role {
    match state {
        ParticipantState::Ready => Role::Success,
        ParticipantState::Starting | ParticipantState::Restarting => Role::Steel,
        ParticipantState::Degraded => Role::Warn,
        ParticipantState::Failed => Role::Error,
        ParticipantState::Stopped => Role::Muted,
    }
}

fn state_symbol(state: ParticipantState) -> &'static str {
    match state {
        ParticipantState::Starting => "…",
        ParticipantState::Ready => "✓",
        ParticipantState::Degraded => "!",
        ParticipantState::Failed => "✗",
        ParticipantState::Restarting => "↻",
        ParticipantState::Stopped => "■",
    }
}

pub(crate) struct StartupView<'a> {
    pub title: &'a TitleInfo,
    pub startup: &'a StartupState,
    pub state: &'a AppState,
    pub telemetry: &'a TelemetrySnapshot,
    pub now: Instant,
}

#[must_use]
pub fn simulation_clock_slot(
    mode: SessionMode,
    clock: Option<Timestamped<ClockSample>>,
    now: Instant,
) -> String {
    match mode {
        SessionMode::Run => "logical real time".to_string(),
        SessionMode::Simulation => clock.map_or_else(
            || "logical n/a".to_string(),
            |sample| {
                let paused = if sample.is_stale(now, DEFAULT_FRESHNESS_TTL) {
                    " · paused"
                } else {
                    ""
                };
                format!(
                    "step {} · {}{paused}",
                    sample.value.step,
                    human::duration(Duration::from_nanos(sample.value.now_ns))
                )
            },
        ),
    }
}

#[must_use]
pub fn device_resource_slot(device: Option<&Timestamped<DeviceSample>>, now: Instant) -> String {
    let Some(device) = device else {
        return "cpu n/a · ram n/a".to_string();
    };
    let stale = if device.is_stale(now, DEFAULT_FRESHNESS_TTL) {
        " · stale"
    } else {
        ""
    };
    let cpu = device
        .value
        .cpu_pct
        .map_or_else(|| "n/a".to_string(), |value| format!("{value:.0}%"));
    let ram = device
        .value
        .ram_used_bytes
        .zip(device.value.ram_total_bytes)
        .map_or_else(
            || "n/a".to_string(),
            |(used, total)| {
                format!(
                    "{}/{}",
                    human::bytes_compact(used),
                    human::bytes_compact(total)
                )
            },
        );
    format!("cpu {cpu} · ram {ram}{stale}",)
}

pub fn draw(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    state: &mut AppState,
    model: &SessionViewModel<'_>,
) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    if draw_too_small(frame, theme, state, area) {
        return;
    }

    let expanded_header = area.height >= 21 && area.width >= 80;
    let header_height = if expanded_header { 7 } else { 4 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(
        frame,
        theme,
        title,
        model.telemetry,
        model.now,
        rows[0],
        expanded_header,
    );
    draw_tabs(frame, theme, state, rows[1]);
    match state.page {
        Page::Overview => draw_overview(frame, theme, state, model, rows[2]),
        Page::Runtimes => draw_runtimes(frame, theme, state, model, rows[2]),
        Page::Logs => draw_logs(frame, theme, state, model, rows[2]),
        Page::Bus => draw_bus(frame, theme, state, model, rows[2]),
        Page::Input => draw_input(frame, theme, state, model, rows[2]),
    }
    draw_footer(frame, theme, state, rows[3]);
    if state.show_help {
        draw_help(frame, theme, area);
    }
    if state.show_info {
        draw_session_info(frame, theme, title, model, area);
    }
}

pub(crate) fn draw_startup(frame: &mut Frame, theme: Theme, view: &StartupView<'_>) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    if draw_too_small(frame, theme, view.state, area) {
        return;
    }
    let expanded_header = area.height >= 21 && area.width >= 80;
    let header_height = if expanded_header { 7 } else { 4 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(
        frame,
        theme,
        view.title,
        view.telemetry,
        view.now,
        rows[0],
        expanded_header,
    );
    draw_tabs(frame, theme, view.state, rows[1]);
    let mut lines = Vec::new();
    lines.push(Line::from(format!(
        "manifest   {}",
        sanitize_terminal_text(&view.title.manifest)
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "session    {}",
        view.startup.session_state.label()
    )));
    if let Some(phase) = &view.startup.phase {
        lines.extend(startup_phase_lines(phase));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Starting session · preparation"))
            .wrap(Wrap { trim: true }),
        centered(rows[2], 76, 74),
    );
    draw_footer(frame, theme, view.state, rows[3]);
}

mod chrome;
use chrome::*;
mod overview;
use overview::*;
mod runtimes;
use runtimes::*;
mod logs;
use logs::*;
mod bus;
use bus::*;
mod input_page;
use input_page::*;
mod overlays;
use overlays::*;
mod formatting;
use formatting::*;

#[cfg(test)]
mod tests;
