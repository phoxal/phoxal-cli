//! Drawing: turns [`AppState`] + a [`BoardSnapshot`] + the [`LogStore`]
//! scrollback into ratatui widgets. Pure rendering - no state mutation, no
//! I/O - so every function here can be exercised against a
//! [`ratatui::backend::TestBackend`] in tests without a real terminal. The
//! one exception is the `now: Instant` every telemetry-facing `draw_*`
//! function takes explicitly (never `Instant::now()` internally) so
//! freshness/staleness checks against `stores::telemetry_store::Timestamped`
//! stay deterministic in tests too.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::identity::IdentitySummary;
#[cfg(test)]
use crate::launch_plan::LaunchOwnership;
use crate::session::event::{DiagnosticLevel, PhaseOutcome};
use crate::stores::log_store::LogStore;
use crate::stores::runtime_store::{RuntimeParticipantMetadata, RuntimeStore};
use crate::stores::telemetry_store::{DEFAULT_FRESHNESS_TTL, HostPoint, Timestamped};
use crate::supervisor::{BoardSnapshot, ClockSample, ParticipantState, ParticipantStatus};
use crate::telemetry::{
    HostSample, JoypadDevicesSample, ProcessSample, RouterMetricsSample, TelemetrySnapshot,
    TopicMetric,
};
use crate::theme::{Role, Theme, state_symbol};
use crate::tui::color;
use crate::tui::diagnostics::{DiagnosticEntry, DiagnosticsStore};
use crate::tui::panel::{Panel, panels_for};
use crate::tui::startup::{PhaseRow, StartupState};
use crate::tui::state::{AppState, Focus, NavRow, TrafficSort, View};

/// Static identity shown on the title bar - resolved once at TUI
/// construction, not re-derived every frame.
#[derive(Debug, Clone)]
pub struct TitleInfo {
    pub robot: String,
    pub channel: String,
    pub mode: &'static str,
}

/// Right-aligned insertion point on the title line for the latest simulation
/// step/time sample. Empty in `run` mode or before the first sample arrives.
#[must_use]
pub fn simulation_clock_slot(mode: &str, clock: Option<ClockSample>) -> String {
    if mode != "simulation" {
        return String::new();
    }
    let Some(sample) = clock else {
        return String::new();
    };
    format!(
        "step {} · {}",
        sample.step,
        crate::human::duration(Duration::from_nanos(sample.now_ns))
    )
}

/// Right-aligned insertion point on the status line for the host CPU/RAM
/// meter: `cpu NN% · ram U/T` from the latest `telemetry::Host` sample, or
/// `cpu n/a` when none has arrived yet - `tool-telemetry` may not be in the
/// catalog yet, and that MUST render as a graceful absence, not an error.
/// A sample older than [`DEFAULT_FRESHNESS_TTL`] as of `now` is marked
/// `(stale)` rather than shown as if it were still live - receive-time
/// freshness from `stores::telemetry_store::Timestamped`, not a
/// last-known-value cache.
#[must_use]
pub fn host_resource_slot(host: Option<&Timestamped<HostSample>>, now: Instant) -> String {
    let Some(host) = host else {
        return "cpu n/a".to_string();
    };
    let stale = if host.is_stale(now, DEFAULT_FRESHNESS_TTL) {
        " (stale)"
    } else {
        ""
    };
    format!(
        "cpu {:.0}% · ram {}/{}{stale}",
        host.value.cpu_pct,
        crate::human::bytes_compact(host.value.ram_used_bytes),
        crate::human::bytes_compact(host.value.ram_total_bytes),
    )
}

/// The theme role the host meter should render in - accent/warn/error by the
/// higher of CPU/RAM load (`theme::resource_role`), or muted for the `n/a`/
/// stale case (a stale reading is not a live load signal worth alarming on).
#[must_use]
fn host_resource_role(host: Option<&Timestamped<HostSample>>, now: Instant) -> Role {
    let Some(host) = host else {
        return Role::Muted;
    };
    if host.is_stale(now, DEFAULT_FRESHNESS_TTL) {
        return Role::Muted;
    }
    let cpu_load = f64::from(host.value.cpu_pct) / 100.0;
    let ram_load = if host.value.ram_total_bytes > 0 {
        host.value.ram_used_bytes as f64 / host.value.ram_total_bytes as f64
    } else {
        0.0
    };
    crate::theme::resource_role(cpu_load.max(ram_load))
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    board: &BoardSnapshot,
    logs: &mut LogStore,
    state: &AppState,
    telemetry: &TelemetrySnapshot,
    runtime: &RuntimeStore,
    now: Instant,
    diagnostics: &DiagnosticsStore,
) {
    let area = frame.area();
    // Repaint every cell before composing the next view. In particular this
    // prevents a long log/overview line from surviving underneath a shorter
    // panel after a tab switch or terminal resize.
    frame.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_title_bar(frame, theme, title, telemetry.clock, rows[0]);
    draw_status_line(
        frame,
        theme,
        board,
        telemetry.host.as_ref(),
        diagnostics,
        now,
        rows[1],
    );
    draw_body(
        frame,
        theme,
        board,
        logs,
        state,
        telemetry,
        runtime,
        diagnostics,
        now,
        rows[2],
    );
    draw_footer(frame, theme, state, rows[3]);

    if state.show_help {
        draw_help_overlay(frame, theme, area);
    }
}

/// The dedicated startup surface (Product decisions 1-4): the welcome card,
/// one current active-phase row and the session state label - rendered
/// INSTEAD of the runtime navigator until
/// `startup.show_startup_surface()` goes false (see `tui::startup`'s module
/// docs for the one-way ratchet). A resize simply redraws this same frame -
/// there is no separate stepper/handoff renderer to fall back to.
pub fn draw_startup(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    identity: Option<&IdentitySummary>,
    startup: &StartupState,
) {
    let area = frame.area();
    let card_height = if identity.is_some() { 10 } else { 1 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(card_height),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_welcome_card(frame, theme, identity, rows[0]);
    draw_active_phase_row(frame, theme, startup.phase.as_ref(), rows[2]);
    draw_startup_status_line(frame, theme, title, startup, rows[4]);
}

/// The welcome card (Product decision 4): left-aligned, never centered - a
/// rounded border via ratatui's own `BorderType::Rounded` (matching
/// `identity::render_welcome_card`'s glyphs without reusing its ANSI-styled
/// `String`, which ratatui's cell buffer cannot interpret as escape
/// sequences - every span here carries its color as a ratatui `Style`
/// instead). Collapses to a bare one-line version when no `robot.yaml` was
/// discoverable (`identity` is `None`) - matching
/// `IdentitySummary::discover`'s own best-effort absence handling.
fn draw_welcome_card(
    frame: &mut Frame,
    theme: Theme,
    identity: Option<&IdentitySummary>,
    area: Rect,
) {
    let Some(identity) = identity else {
        frame.render_widget(
            Paragraph::new(format!("phoxal-cli {}", env!("CARGO_PKG_VERSION")))
                .style(color::fg(theme, Role::Accent)),
            area,
        );
        return;
    };
    let lines = vec![
        Line::from(Span::styled(" \u{25c7}", color::fg(theme, Role::Accent))),
        Line::from(vec![
            Span::styled("\u{25c7} \u{25c7}   ", color::fg(theme, Role::Accent)),
            Span::styled(
                "p h o x a l",
                color::fg(theme, Role::TextPrimary).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "\u{25c7}\u{25c7}\u{25c7}\u{25c7}\u{25c7}    ",
                color::fg(theme, Role::Accent),
            ),
            Span::styled(
                format!("phoxal-cli {}", env!("CARGO_PKG_VERSION")),
                color::muted(theme),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("robot     ", color::muted(theme)),
            Span::styled(identity.robot.clone(), color::fg(theme, Role::TextPrimary)),
        ]),
        Line::from(vec![
            Span::styled("manifest  ", color::muted(theme)),
            Span::styled(
                identity.manifest.clone(),
                color::fg(theme, Role::TextPrimary),
            ),
        ]),
        Line::from(vec![
            Span::styled("channel   ", color::muted(theme)),
            Span::styled(
                identity.channel.clone(),
                color::fg(theme, Role::TextPrimary),
            ),
        ]),
        Line::from(vec![
            Span::styled("terminal  ", color::muted(theme)),
            Span::styled(
                identity.terminal.label(),
                color::fg(theme, Role::TextPrimary),
            ),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(color::fg(theme, Role::Border));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Left),
        area,
    );
}

/// The one current active-phase row (Product decision 3): running (with
/// progress if carried), or a compact completion result once finished. Blank
/// when no phase has started yet - never a placeholder for work that has not
/// begun.
fn draw_active_phase_row(frame: &mut Frame, theme: Theme, phase: Option<&PhaseRow>, area: Rect) {
    let Some(phase) = phase else {
        frame.render_widget(Paragraph::new(""), area);
        return;
    };
    let line = match &phase.outcome {
        Some((PhaseOutcome::Succeeded, elapsed)) => Line::from(vec![
            Span::styled("\u{2713} ", color::fg(theme, Role::Success)),
            Span::styled(
                format!("{} ({})", phase.label, crate::human::duration(*elapsed)),
                color::fg(theme, Role::TextPrimary),
            ),
        ]),
        Some((PhaseOutcome::Skipped, _)) => Line::from(Span::styled(
            format!("\u{b7} {}", phase.label),
            color::muted(theme),
        )),
        Some((PhaseOutcome::Failed { error }, _)) => Line::from(Span::styled(
            format!("\u{2717} {} - {error}", phase.label),
            color::fg(theme, Role::Error),
        )),
        None => {
            let progress_text = phase
                .progress
                .as_ref()
                .map_or_else(String::new, |progress| {
                    let counter = if progress.total > 0 {
                        format!(" ({}/{})", progress.completed, progress.total)
                    } else {
                        String::new()
                    };
                    let detail = progress
                        .detail
                        .as_ref()
                        .map_or_else(String::new, |detail| format!(" - {detail}"));
                    format!("{counter}{detail}")
                });
            Line::from(vec![
                Span::styled("\u{2026} ", color::fg(theme, Role::Accent)),
                Span::styled(
                    format!("{}{progress_text}", phase.label),
                    color::fg(theme, Role::TextPrimary),
                ),
            ])
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// The startup surface's bottom line: the mode plus the session's own human
/// label (`"preparing"`, `"starting"`, `"running"`, ...) - the plain
/// data `SessionState::label` already computes, not re-derived here.
fn draw_startup_status_line(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    startup: &StartupState,
    area: Rect,
) {
    let text = format!("{} \u{b7} {}", title.mode, startup.session_state.label());
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, color::muted(theme)))),
        area,
    );
}

/// Build one line with `left` flush to the start and `right` flush to the
/// end, padded with plain spaces in between (or truncated if both together
/// overflow `width`) - the shared "insertion-point" layout for the title
/// and status rows.
fn split_line(width: u16, left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
    let width = width as usize;
    let left_width: usize = left.iter().map(|span| span.content.width()).sum();
    let right_width: usize = right.iter().map(|span| span.content.width()).sum();
    let mut spans = left;
    if right_width > 0 && left_width + right_width < width {
        let gap = width - left_width - right_width;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    Line::from(spans)
}

fn draw_title_bar(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    clock: Option<ClockSample>,
    area: Rect,
) {
    let left = vec![
        Span::styled("phoxal", color::fg(theme, Role::Accent)),
        Span::raw(" · "),
        Span::styled(title.robot.clone(), color::fg(theme, Role::TextPrimary)),
        Span::raw(" · "),
        Span::styled(title.channel.clone(), color::fg(theme, Role::TextPrimary)),
        Span::raw(" · "),
        Span::styled(title.mode, color::fg(theme, Role::TextPrimary)),
    ];
    let clock_text = simulation_clock_slot(title.mode, clock);
    let right = if clock_text.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(clock_text, color::muted(theme))]
    };
    let line = split_line(area.width, left, right);
    frame.render_widget(
        Paragraph::new(line).style(color::fg(theme, Role::Text)),
        area,
    );
}

fn draw_status_line(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    host: Option<&Timestamped<HostSample>>,
    diagnostics: &DiagnosticsStore,
    now: Instant,
    area: Rect,
) {
    let total = board.participants.len();
    let connected = board
        .participants
        .values()
        .filter(|status| status.state == ParticipantState::Ready)
        .count();
    let degraded = board
        .participants
        .values()
        .filter(|status| {
            matches!(
                status.state,
                ParticipantState::Degraded | ParticipantState::Failed
            )
        })
        .count();
    let mut left = vec![Span::styled(
        format!("{connected}/{total} connected · {degraded} degraded"),
        color::fg(theme, Role::TextPrimary),
    )];
    if !diagnostics.is_empty() {
        let role = if diagnostics.error_count() > 0 {
            Role::Error
        } else {
            Role::Warn
        };
        left.push(Span::raw(" · "));
        left.push(Span::styled(
            format!("diagnostics {}", diagnostics.len()),
            color::fg(theme, role).add_modifier(Modifier::BOLD),
        ));
    }
    let right = vec![Span::styled(
        host_resource_slot(host, now),
        color::fg(theme, host_resource_role(host, now)),
    )];
    let line = split_line(area.width, left, right);
    frame.render_widget(Paragraph::new(line), area);
}

#[allow(clippy::too_many_arguments)]
fn draw_body(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    logs: &mut LogStore,
    state: &AppState,
    telemetry: &TelemetrySnapshot,
    runtime: &RuntimeStore,
    diagnostics: &DiagnosticsStore,
    now: Instant,
    area: Rect,
) {
    let nav_width = (area.width / 3).clamp(16, 36).min(area.width);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(nav_width), Constraint::Min(0)])
        .split(area);
    draw_navigator(frame, theme, board, state, columns[0]);
    draw_right_pane(
        frame,
        theme,
        board,
        logs,
        state,
        telemetry,
        runtime,
        diagnostics,
        now,
        columns[1],
    );
}

fn draw_navigator(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    state: &AppState,
    area: Rect,
) {
    let active = state.focus == Focus::Navigator;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(if active {
            color::fg(theme, Role::Accent)
        } else {
            color::muted(theme)
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.saturating_sub(1) as usize;
    let items: Vec<ListItem> = state
        .navigator
        .rows
        .iter()
        .map(|row| navigator_row_item(theme, board, row, width, active))
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(state.navigator.cursor));
    let list = if active {
        List::new(items)
            .highlight_style(color::selected(theme, Role::Text))
            .highlight_symbol("▍")
    } else {
        List::new(items)
            .highlight_style(color::fg(theme, Role::MutedStrong))
            .highlight_symbol(" ")
    };
    frame.render_stateful_widget(list, inner, &mut list_state);
}

fn navigator_row_item<'a>(
    theme: Theme,
    board: &BoardSnapshot,
    row: &NavRow,
    width: usize,
    active: bool,
) -> ListItem<'a> {
    match row {
        NavRow::Header(group) => ListItem::new(Line::from(Span::styled(
            format!(" {}", group.label()),
            color::muted(theme).add_modifier(Modifier::BOLD),
        ))),
        NavRow::Participant(id) => {
            let role = if active {
                board
                    .participants
                    .get(id)
                    .map_or(Role::TextPrimary, |status| {
                        crate::theme::state_role(status.state)
                    })
            } else {
                Role::MutedStrong
            };
            let text = board
                .participants
                .get(id)
                .map_or_else(|| id.clone(), |status| participant_row_text(status, width));
            ListItem::new(Line::from(Span::styled(
                format!(" {text}"),
                color::fg(theme, role),
            )))
        }
    }
}

/// Render one participant row's full text (state SYMBOL and TEXT label,
/// local marker, restart count, id) - split out from `navigator_row_item` so
/// it can be unit-tested for unicode-width truncation without a `Frame`.
/// Carries both the symbol AND the label (design doc part 7) so the state is
/// legible even where the symbol glyph alone would be ambiguous.
#[must_use]
pub fn participant_row_text(status: &ParticipantStatus, width: usize) -> String {
    let symbol = state_symbol(status.state);
    let label = status.state.label();
    let local = if status.local { "*" } else { " " };
    let restarts = if status.restart_count > 0 {
        format!(" ↻{}", status.restart_count)
    } else {
        String::new()
    };
    let text = format!("{symbol} {label} {local}{}{restarts}", status.id);
    truncate_to_width(&text, width)
}

/// Truncate `text` to at most `width` display columns (unicode-width aware,
/// not byte/char-count aware), appending an ellipsis marker when truncated so
/// the cut is visible rather than silent.
#[must_use]
pub fn truncate_to_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]) as &str);
        if used + ch_width > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

#[allow(clippy::too_many_arguments)]
fn draw_right_pane(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    logs: &mut LogStore,
    state: &AppState,
    telemetry: &TelemetrySnapshot,
    runtime: &RuntimeStore,
    diagnostics: &DiagnosticsStore,
    now: Instant,
    area: Rect,
) {
    frame.render_widget(Clear, area);
    match &state.view {
        View::Home | View::Diagnostics => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(area);
            let titles = vec![
                Line::from("Overview"),
                Line::from(format!("Diagnostics ({})", diagnostics.len())),
            ];
            let selected = usize::from(state.view == View::Diagnostics);
            let tabs = Tabs::new(titles)
                .select(selected)
                .style(color::muted(theme));
            let tabs = if state.focus == Focus::Detail {
                tabs.highlight_style(
                    color::fg(theme, Role::Accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                )
            } else {
                tabs.highlight_style(color::fg(theme, Role::MutedStrong))
            };
            frame.render_widget(tabs, rows[0]);
            if state.view == View::Diagnostics {
                draw_diagnostics_tab(frame, theme, diagnostics, state, rows[1]);
            } else {
                draw_overview_home(frame, theme, board, rows[1]);
            }
        }
        View::Runtime(id) => {
            draw_runtime_detail(
                frame, theme, board, logs, state, telemetry, runtime, now, id, area,
            );
        }
    }
}

fn draw_overview_home(frame: &mut Frame, theme: Theme, board: &BoardSnapshot, area: Rect) {
    let starting: Vec<&str> = board
        .participants
        .values()
        .filter(|status| matches!(status.state, ParticipantState::Starting))
        .map(|status| status.id.as_str())
        .collect();
    let unhealthy: Vec<&str> = board
        .participants
        .values()
        .filter(|status| {
            matches!(
                status.state,
                ParticipantState::Failed | ParticipantState::Degraded
            )
        })
        .map(|status| status.id.as_str())
        .collect();
    let restarted: Vec<&str> = board
        .participants
        .values()
        .filter(|status| status.restart_count > 0)
        .map(|status| status.id.as_str())
        .collect();
    let suggested =
        crate::tui::groups::suggested_participant(board).map(|status| status.id.as_str());

    let operational = unhealthy.is_empty() && starting.is_empty() && !board.participants.is_empty();

    let mut lines = vec![
        Line::from(Span::styled(
            if operational {
                "Operational"
            } else {
                "Not fully operational"
            },
            color::fg(
                theme,
                if operational {
                    Role::Success
                } else {
                    Role::Warn
                },
            ),
        )),
        Line::from(""),
    ];
    lines.push(Line::from(Span::styled(
        format!("Starting: {}", join_or_none(&starting)),
        color::fg(theme, Role::TextPrimary),
    )));
    lines.push(Line::from(Span::styled(
        format!("Unhealthy: {}", join_or_none(&unhealthy)),
        color::fg(theme, Role::TextPrimary),
    )));
    lines.push(Line::from(Span::styled(
        format!("Restarted: {}", join_or_none(&restarted)),
        color::fg(theme, Role::TextPrimary),
    )));
    lines.push(Line::from(""));
    if let Some(id) = suggested {
        lines.push(Line::from(vec![
            Span::styled("Suggested: ", color::fg(theme, Role::Accent)),
            Span::styled(id, color::fg(theme, Role::Text)),
        ]));
    }

    let block = Block::default().title(" Overview ").borders(Borders::NONE);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_diagnostics_tab(
    frame: &mut Frame,
    theme: Theme,
    diagnostics: &DiagnosticsStore,
    state: &AppState,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(area);

    let mut summary = format!(
        "{} · {} warning{} · {} error{}",
        state.diagnostics.severity.label(),
        diagnostics.warning_count(),
        if diagnostics.warning_count() == 1 {
            ""
        } else {
            "s"
        },
        diagnostics.error_count(),
        if diagnostics.error_count() == 1 {
            ""
        } else {
            "s"
        },
    );
    if !state.diagnostics.filter.is_empty() {
        summary.push_str(&format!(" · filter: {}", state.diagnostics.filter));
    }
    if diagnostics.dropped_count() > 0 {
        summary.push_str(&format!(" · {} older dropped", diagnostics.dropped_count()));
    }
    frame.render_widget(
        Paragraph::new(truncate_to_width(&summary, rows[0].width as usize))
            .style(color::muted(theme)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new("TIME      LEVEL  SOURCE        MESSAGE")
            .style(color::muted(theme).add_modifier(Modifier::BOLD)),
        rows[1],
    );

    let query = state.diagnostics.filter.to_lowercase();
    let matching: Vec<&DiagnosticEntry> = diagnostics
        .entries()
        .iter()
        .filter(|entry| state.diagnostics.severity.matches(entry.level))
        .filter(|entry| {
            query.is_empty()
                || entry.source.label().to_lowercase().contains(&query)
                || entry.message.to_lowercase().contains(&query)
        })
        .collect();

    if matching.is_empty() {
        let message = if diagnostics.is_empty() {
            "No warnings or errors."
        } else {
            "No diagnostics match the current filters."
        };
        frame.render_widget(Paragraph::new(message).style(color::muted(theme)), rows[2]);
        return;
    }

    let height = rows[2].height as usize;
    let max_start = matching.len().saturating_sub(height);
    let start = max_start.saturating_sub(state.diagnostics.scroll.min(max_start));
    let end = (start + height).min(matching.len());
    let lines = matching[start..end]
        .iter()
        .map(|entry| diagnostic_line(theme, entry, rows[2].width as usize))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), rows[2]);
}

fn diagnostic_line(theme: Theme, entry: &DiagnosticEntry, width: usize) -> Line<'static> {
    let elapsed = crate::human::duration(entry.elapsed);
    let (level, role) = match entry.level {
        DiagnosticLevel::Warn => ("WARN ", Role::Warn),
        DiagnosticLevel::Error => ("ERROR", Role::Error),
        DiagnosticLevel::Info => ("INFO ", Role::TextPrimary),
    };
    let source = truncate_to_width(entry.source.label(), 12);
    let prefix = format!("+{elapsed:>8}  {level}  {source:<12}  ");
    let message_width = width.saturating_sub(prefix.width());
    Line::from(vec![
        Span::styled(prefix, color::fg(theme, role)),
        Span::styled(
            truncate_to_width(&entry.message, message_width),
            color::fg(theme, Role::TextPrimary),
        ),
    ])
}

fn join_or_none(ids: &[&str]) -> String {
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.join(", ")
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_runtime_detail(
    frame: &mut Frame,
    theme: Theme,
    board: &BoardSnapshot,
    logs: &mut LogStore,
    state: &AppState,
    telemetry: &TelemetrySnapshot,
    runtime: &RuntimeStore,
    now: Instant,
    id: &str,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let panels = panels_for(id);
    let titles: Vec<Line> = panels
        .iter()
        .map(|panel| Line::from(panel.label()))
        .collect();
    let selected = panels
        .iter()
        .position(|panel| *panel == state.panel)
        .unwrap_or(0);
    let tabs_widget = Tabs::new(titles)
        .select(selected)
        .highlight_style(
            color::fg(theme, Role::Accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .style(color::muted(theme));
    frame.render_widget(tabs_widget, rows[0]);

    let Some(status) = board.participants.get(id) else {
        frame.render_widget(
            Paragraph::new(format!("{id} is no longer on the board")).style(color::muted(theme)),
            rows[1],
        );
        return;
    };

    match state.panel {
        Panel::Overview => draw_runtime_overview(
            frame,
            theme,
            status,
            telemetry.process_by_participant.get(id),
            runtime.metadata(id),
            runtime.time_to_ready(id),
            operator_state_line(id, telemetry, now),
            now,
            state.overview.scroll,
            rows[1],
        ),
        Panel::Logs => draw_runtime_logs(frame, theme, logs, state, id, rows[1]),
        Panel::Traffic => {
            draw_traffic_tab(
                frame,
                theme,
                state,
                telemetry.router.as_ref(),
                runtime,
                now,
                rows[1],
            );
        }
        Panel::Devices => {
            draw_joypad_tab(frame, theme, state, telemetry.joypad.as_ref(), now, rows[1]);
        }
        Panel::Resources => draw_resources_tab(
            frame,
            theme,
            state,
            telemetry.host.as_ref(),
            &telemetry.host_history,
            now,
            rows[1],
        ),
    }
}

/// A field the Overview panel falls back to when [`RuntimeStore`] genuinely
/// has no metadata for this participant (finding A5's own fallback for the
/// case the sidecar itself documents as "should not happen for anything the
/// launch plan named" - e.g. a stale board entry from a previous session).
/// Never fabricate a value in its place.
const UNKNOWN_FIELD: &str = "unknown (not in this session's launch plan)";

fn format_contract_list(contracts: &[String]) -> String {
    if contracts.is_empty() {
        "(none)".to_string()
    } else {
        contracts.join(", ")
    }
}

fn format_time_to_ready(time_to_ready: Option<Duration>) -> String {
    time_to_ready.map_or_else(
        || "waiting for first ready".to_string(),
        crate::human::duration,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_runtime_overview(
    frame: &mut Frame,
    theme: Theme,
    status: &ParticipantStatus,
    process: Option<&Timestamped<ProcessSample>>,
    runtime_metadata: Option<&RuntimeParticipantMetadata>,
    time_to_ready: Option<Duration>,
    operator_state: Option<String>,
    now: Instant,
    scroll: usize,
    area: Rect,
) {
    let cpu_text = process.map_or_else(
        || "n/a".to_string(),
        |sample| {
            let stale = if sample.is_stale(now, DEFAULT_FRESHNESS_TTL) {
                " (stale)"
            } else {
                ""
            };
            format!("{:.1}%{stale}", sample.value.cpu_pct)
        },
    );
    let ram_text = process.map_or_else(
        || "n/a".to_string(),
        |sample| crate::human::bytes(sample.value.rss_bytes),
    );
    let artifact_text = runtime_metadata.and_then(|meta| meta.artifact_ref.clone());
    let ownership_text = runtime_metadata.map(|meta| match meta.ownership {
        crate::stores::runtime_store::RuntimeOwnership::CliManaged => "cli-managed".to_string(),
        crate::stores::runtime_store::RuntimeOwnership::SimulationManaged => {
            "simulation-managed".to_string()
        }
        crate::stores::runtime_store::RuntimeOwnership::External => "external".to_string(),
    });
    let input_contracts_text =
        runtime_metadata.map(|meta| format_contract_list(&meta.input_contracts));
    let output_contracts_text =
        runtime_metadata.map(|meta| format_contract_list(&meta.output_contracts));
    let time_to_ready_text = runtime_metadata.map(|_| format_time_to_ready(time_to_ready));
    let mut lines = vec![
        Line::from(vec![
            Span::styled("state     ", color::muted(theme)),
            Span::raw(theme.participant_state(status.state)),
        ]),
        Line::from(vec![
            Span::styled("kind      ", color::muted(theme)),
            Span::raw(status.kind.label()),
        ]),
        Line::from(vec![
            Span::styled("local     ", color::muted(theme)),
            Span::raw(if status.local { "yes" } else { "no" }),
        ]),
        Line::from(vec![
            Span::styled("restarts  ", color::muted(theme)),
            Span::raw(status.restart_count.to_string()),
        ]),
        Line::from(vec![
            Span::styled("cpu       ", color::muted(theme)),
            Span::raw(cpu_text),
        ]),
        Line::from(vec![
            Span::styled("ram       ", color::muted(theme)),
            Span::raw(ram_text),
        ]),
        Line::from(vec![
            Span::styled("artifact  ", color::muted(theme)),
            Span::raw(artifact_text.unwrap_or_else(|| UNKNOWN_FIELD.to_string())),
        ]),
        Line::from(vec![
            Span::styled("input     ", color::muted(theme)),
            Span::raw(input_contracts_text.unwrap_or_else(|| UNKNOWN_FIELD.to_string())),
        ]),
        Line::from(vec![
            Span::styled("output    ", color::muted(theme)),
            Span::raw(output_contracts_text.unwrap_or_else(|| UNKNOWN_FIELD.to_string())),
        ]),
        Line::from(vec![
            Span::styled("ownership ", color::muted(theme)),
            Span::raw(ownership_text.unwrap_or_else(|| UNKNOWN_FIELD.to_string())),
        ]),
        Line::from(vec![
            Span::styled("time to ready ", color::muted(theme)),
            Span::raw(time_to_ready_text.unwrap_or_else(|| UNKNOWN_FIELD.to_string())),
        ]),
        Line::from(vec![
            Span::styled("last error ", color::muted(theme)),
            Span::raw(status.note.clone().unwrap_or_else(|| "-".to_string())),
        ]),
    ];
    if let Some(operator_state) = operator_state {
        lines.insert(
            1,
            Line::from(vec![
                Span::styled("runtime   ", color::muted(theme)),
                Span::raw(operator_state),
            ]),
        );
    }
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

fn operator_state_line(id: &str, telemetry: &TelemetrySnapshot, now: Instant) -> Option<String> {
    let stale_suffix = |received_at: Instant| {
        if now.duration_since(received_at) > DEFAULT_FRESHNESS_TTL {
            " (stale)"
        } else {
            ""
        }
    };
    if id == "motion" {
        return telemetry.motion.as_ref().map(|sample| {
            let safety_reason = sample
                .value
                .active_safety_constraints
                .first()
                .map(|constraint| format!(" safety={:?}", constraint.reason))
                .unwrap_or_default();
            format!(
                "source={:?} target=({:.2} m/s, {:.2} rad/s) zero={:?}{}{}",
                sample.value.selected_source,
                sample.value.final_target.linear_x_mps,
                sample.value.final_target.angular_z_radps,
                sample.value.zero_reason,
                safety_reason,
                stale_suffix(sample.received_at),
            )
        });
    }
    if id == "drive" {
        return telemetry.drive.as_ref().map(|sample| {
            format!(
                "authority={:?} target=({:.2} m/s, {:.2} rad/s) stop={:?}{}",
                sample.value.actuator_authority,
                sample.value.limited_target.linear_x_mps,
                sample.value.limited_target.angular_z_radps,
                sample.value.stop_reason,
                stale_suffix(sample.received_at),
            )
        });
    }
    None
}

fn draw_runtime_logs(
    frame: &mut Frame,
    theme: Theme,
    logs: &mut LogStore,
    state: &AppState,
    id: &str,
    area: Rect,
) {
    let filter = state.logs.filter.to_lowercase();
    let lines: Vec<Line> = logs
        .lines_for(id)
        .iter()
        .filter(|line| filter.is_empty() || line.text.to_lowercase().contains(&filter))
        .map(|line| {
            let tag = match line.source {
                crate::supervisor::LogSource::Bus => "",
                crate::supervisor::LogSource::Raw => "[raw] ",
            };
            Line::from(Span::styled(
                format!("{tag}{}", line.text),
                color::fg(theme, Role::TextPrimary),
            ))
        })
        .collect();

    let height = area.height as usize;
    let scroll = if state.logs.follow {
        lines.len().saturating_sub(height)
    } else {
        state
            .logs
            .scroll
            .min(lines.len().saturating_sub(height.min(lines.len())))
    };
    let paragraph = Paragraph::new(lines).scroll((scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

/// A topic's ingress rate has dropped to effectively zero over the router's
/// measurement window - rendered as `stale` rather than `live` in the
/// Traffic table's STATUS column.
const STALE_RATE_THRESHOLD_HZ: f32 = 0.01;

#[must_use]
fn topic_is_stale(topic: &TopicMetric) -> bool {
    topic.ingress_rate_hz < STALE_RATE_THRESHOLD_HZ
}

fn sort_traffic_rows(topics: &mut [&TopicMetric], sort: TrafficSort) {
    match sort {
        TrafficSort::Rate => {
            topics.sort_by(|left, right| right.ingress_rate_hz.total_cmp(&left.ingress_rate_hz));
        }
        TrafficSort::Topic => topics.sort_by(|left, right| left.topic.cmp(&right.topic)),
        TrafficSort::Producer => {
            topics.sort_by(|left, right| left.from_participant.cmp(&right.from_participant));
        }
        TrafficSort::Status => topics.sort_by(|left, right| {
            topic_is_stale(right)
                .cmp(&topic_is_stale(left))
                .then_with(|| right.ingress_rate_hz.total_cmp(&left.ingress_rate_hz))
        }),
    }
}

/// The router Traffic tab: a sortable (`s`), filterable (`/`) table of every
/// topic the router has measured ingress for - TOPIC, PRODUCER, RATE, COUNT,
/// a live/stale STATUS derived from the measured rate itself (design doc:
/// "stale if a topic's rate dropped to ~0 over the window"), and a
/// locally-known CONSUMERS count (finding A5 - see
/// `RuntimeStore::potential_consumers`'s own docs for the exact
/// approximation: a count of participants whose declared input contract
/// name appears in the topic's own composed wire key, not a live
/// subscription registry). Header line carries the router's own aggregate
/// `throughput_msg_s` plus the SAMPLE's own receive-time freshness (distinct
/// from a topic's own rate-derived staleness below: this is "is the router
/// still publishing `Metrics` at all", that is "is THIS topic's ingress
/// still flowing").
fn draw_traffic_tab(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    sample: Option<&Timestamped<RouterMetricsSample>>,
    runtime: &RuntimeStore,
    now: Instant,
    area: Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let Some(sample) = sample else {
        frame.render_widget(
            Paragraph::new("loading router traffic…").style(color::muted(theme)),
            area,
        );
        return;
    };
    let stale = sample.is_stale(now, DEFAULT_FRESHNESS_TTL);
    let sample = &sample.value;

    frame.render_widget(
        Paragraph::new(format!(
            "throughput {:.1} msg/s · {} topic(s) · sort:{}{}",
            sample.throughput_msg_s,
            sample.topics.len(),
            state.traffic.sort.label(),
            if stale { " · feed stale" } else { "" },
        ))
        .style(color::muted(theme)),
        rows[0],
    );

    let filter = state.traffic.filter.to_lowercase();
    let mut topics: Vec<&TopicMetric> = sample
        .topics
        .iter()
        .filter(|topic| {
            filter.is_empty()
                || topic.topic.to_lowercase().contains(&filter)
                || topic.from_participant.to_lowercase().contains(&filter)
        })
        .collect();
    sort_traffic_rows(&mut topics, state.traffic.sort);

    let width = rows[1].width as usize;
    let mut lines = vec![Line::from(Span::styled(
        traffic_header_row(width),
        color::muted(theme).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(topics.iter().map(|topic| {
        traffic_row_line(
            theme,
            topic,
            runtime.potential_consumers(&topic.topic),
            width,
        )
    }));

    let height = rows[1].height as usize;
    let scroll = state
        .traffic
        .scroll
        .min(lines.len().saturating_sub(height.min(lines.len())));
    let paragraph = Paragraph::new(lines).scroll((scroll as u16, 0));
    frame.render_widget(paragraph, rows[1]);
}

fn traffic_header_row(width: usize) -> String {
    truncate_to_width(
        &format!(
            "{:<28} {:<16} {:>8} {:>9} {:>6} {:>9}",
            "TOPIC", "PRODUCER", "RATE", "COUNT", "STATUS", "CONSUMERS"
        ),
        width,
    )
}

fn traffic_row_line(
    theme: Theme,
    topic: &TopicMetric,
    consumers: usize,
    width: usize,
) -> Line<'static> {
    let stale = topic_is_stale(topic);
    let producer = if topic.from_participant.is_empty() {
        "-"
    } else {
        topic.from_participant.as_str()
    };
    let text = format!(
        "{:<28} {:<16} {:>6.1}Hz {:>9} {:>6} {:>9}",
        truncate_to_width(&topic.topic, 28),
        truncate_to_width(producer, 16),
        topic.ingress_rate_hz,
        topic.count,
        if stale { "stale" } else { "live" },
        consumers,
    );
    let role = if stale { Role::Muted } else { Role::Success };
    Line::from(Span::styled(
        truncate_to_width(&text, width),
        color::fg(theme, role),
    ))
}

/// The joypad Devices tab: pure list navigation over
/// `telemetry::JoypadDevicesSample::available` (`↑↓` moves
/// `AppState::devices.cursor`, a purely local UI cursor), with the
/// AUTHORITATIVE `selected` device (from the tool's own last ack, never a
/// local guess) marked separately from the cursor highlight, and the
/// sample's own receive-time freshness/error state surfaced alongside it.
fn draw_joypad_tab(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    sample: Option<&Timestamped<JoypadDevicesSample>>,
    now: Instant,
    area: Rect,
) {
    let Some(sample) = sample else {
        frame.render_widget(
            Paragraph::new("loading joypad devices…").style(color::muted(theme)),
            area,
        );
        return;
    };
    let stale = sample.is_stale(now, DEFAULT_FRESHNESS_TTL);
    let sample = &sample.value;

    let mut lines = vec![Line::from(Span::styled(
        "✓ selected automatically · L1/R1 forward · L2/R2 reverse",
        color::muted(theme),
    ))];
    if stale {
        lines.push(Line::from(Span::styled(
            "stale - tool-joypad has not published recently",
            color::fg(theme, Role::Warn),
        )));
    }
    if sample.available.is_empty() {
        lines.push(Line::from(Span::styled(
            "no gamepads detected",
            color::muted(theme),
        )));
    }
    for (index, device) in sample.available.iter().enumerate() {
        let dot = if device.connected { "●" } else { "○" };
        let marker = if sample.selected.as_deref() == Some(device.id.as_str()) {
            "✓"
        } else {
            " "
        };
        let cursor = if index == state.devices.cursor {
            "▍"
        } else {
            " "
        };
        let role = if index == state.devices.cursor {
            Role::Accent
        } else if device.connected {
            Role::TextPrimary
        } else {
            Role::Muted
        };
        lines.push(Line::from(Span::styled(
            format!("{cursor}{marker} {dot} {}", device.name),
            color::fg(theme, role),
        )));
    }
    if let Some(error) = &sample.last_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            color::fg(theme, Role::Error),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// One 9-level sparkline block per value in `[0.0, 1.0]` (clamped), oldest
/// first - a compact rolling-history readout that still degrades to a plain
/// glyph run under `ColorCapability::None` (the fill level IS the signal, not
/// the color, matching `theme::resource_meter`'s convention).
const SPARK_LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn sparkline_line(theme: Theme, values: impl Iterator<Item = f64>) -> Line<'static> {
    let text: String = values
        .map(|value| {
            let clamped = value.clamp(0.0, 1.0);
            let index = (clamped * (SPARK_LEVELS.len() - 1) as f64).round() as usize;
            SPARK_LEVELS[index.min(SPARK_LEVELS.len() - 1)]
        })
        .collect();
    if text.is_empty() {
        Line::from(Span::styled("no samples yet", color::muted(theme)))
    } else {
        Line::from(Span::styled(text, color::fg(theme, Role::Accent)))
    }
}

/// The `tool-telemetry` Resources tab: the latest `Host` sample's headline
/// numbers plus a rolling cpu%/ram% sparkline history from
/// `stores::telemetry_store::TelemetryStore` (design doc Part 3c/4d/7),
/// windowed to `state.resources.range` and frozen at the moment of pausing
/// (`p`) via `ResourcesState::display_host`/`display_history` - see that
/// type's docs.
fn draw_resources_tab(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    host: Option<&Timestamped<HostSample>>,
    history: &[HostPoint],
    now: Instant,
    area: Rect,
) {
    let live_host = host.map(|sample| sample.value);
    let stale = host.is_some_and(|sample| sample.is_stale(now, DEFAULT_FRESHNESS_TTL));
    let Some(host) = state.resources.display_host(live_host.as_ref()).copied() else {
        frame.render_widget(
            Paragraph::new("loading host resources…").style(color::muted(theme)),
            area,
        );
        return;
    };
    let ram_pct = if host.ram_total_bytes > 0 {
        host.ram_used_bytes as f64 / host.ram_total_bytes as f64 * 100.0
    } else {
        0.0
    };
    let displayed_history = state.resources.display_history(history);
    let window_secs = state.resources.range.seconds();
    let windowed: Vec<&HostPoint> = displayed_history
        .iter()
        .rev()
        .take_while(|point| {
            now.saturating_duration_since(point.received_at).as_secs() <= window_secs
        })
        .collect();
    let mut status = Vec::new();
    if state.resources.paused {
        status.push("paused".to_string());
    }
    if stale {
        status.push("stale".to_string());
    }
    status.push(format!("window {}", state.resources.range.label()));
    let mut lines = vec![
        Line::from(Span::styled(status.join(" · "), color::muted(theme))),
        Line::from(vec![
            Span::styled("cpu    ", color::muted(theme)),
            Span::raw(format!("{:.0}%", host.cpu_pct)),
        ]),
        Line::from(vec![
            Span::styled("ram    ", color::muted(theme)),
            Span::raw(format!(
                "{} / {} ({ram_pct:.0}%)",
                crate::human::bytes(host.ram_used_bytes),
                crate::human::bytes(host.ram_total_bytes),
            )),
        ]),
        Line::from(vec![
            Span::styled("load 1m ", color::muted(theme)),
            Span::raw(format!("{:.2}", host.load_1m)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("cpu history ({})", state.resources.range.label()),
            color::muted(theme),
        )),
        sparkline_line(
            theme,
            windowed
                .iter()
                .rev()
                .map(|point| f64::from(point.cpu_pct) / 100.0),
        ),
        Line::from(Span::styled(
            format!("ram history ({})", state.resources.range.label()),
            color::muted(theme),
        )),
        sparkline_line(
            theme,
            windowed.iter().rev().map(|point| {
                if point.ram_total_bytes > 0 {
                    point.ram_used_bytes as f64 / point.ram_total_bytes as f64
                } else {
                    0.0
                }
            }),
        ),
    ];
    lines.push(Line::from(""));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame, theme: Theme, state: &AppState, area: Rect) {
    let segments: Vec<&str> = footer_segments(state);
    let mut text = segments.join("  ");
    text = truncate_to_width(&text, area.width as usize);
    frame.render_widget(Paragraph::new(text).style(color::muted(theme)), area);
}

fn footer_segments(state: &AppState) -> Vec<&'static str> {
    if state.is_filtering() {
        return vec!["type to filter", "↵/Esc done"];
    }
    if state.view == View::Diagnostics {
        return vec![
            "↑↓ scroll",
            "|",
            "s severity",
            "/ filter",
            "d/Esc back",
            "? help",
        ];
    }
    match state.focus {
        Focus::Navigator => vec![
            "↑↓ select",
            "|",
            "↵ inspect",
            "r restart",
            "/ filter",
            "d diagnostics",
            "? help",
            "q quit",
        ],
        Focus::Detail if state.panel == Panel::Logs => {
            vec![
                "↑↓ scroll",
                "|",
                "f follow",
                "/ filter",
                "←→ tab",
                "d diagnostics",
                "Esc back",
                "? help",
            ]
        }
        Focus::Detail if state.panel == Panel::Traffic => vec![
            "↑↓ scroll",
            "|",
            "s sort",
            "/ filter",
            "←→ tab",
            "d diagnostics",
            "Esc back",
            "? help",
        ],
        Focus::Detail if state.panel == Panel::Devices => vec![
            "↑↓ select",
            "|",
            "↵ connect",
            "r rescan",
            "←→ tab",
            "d diagnostics",
            "Esc back",
            "? help",
        ],
        Focus::Detail if state.panel == Panel::Resources => vec![
            "p pause",
            "|",
            "w window",
            "←→ tab",
            "d diagnostics",
            "Esc back",
            "? help",
        ],
        Focus::Detail => vec![
            "↑↓ scroll",
            "|",
            "←→ tab",
            "d diagnostics",
            "Esc back",
            "? help",
        ],
    }
}

fn draw_help_overlay(frame: &mut Frame, theme: Theme, area: Rect) {
    let width = (area.width.saturating_sub(4)).min(60);
    let height = 13.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from("Keys"),
        Line::from(""),
        Line::from("↑↓        move / scroll"),
        Line::from("↵         inspect selected runtime"),
        Line::from("←→        switch tab"),
        Line::from("r         restart selected runtime"),
        Line::from("d         diagnostics tab"),
        Line::from("/         filter"),
        Line::from("f         toggle log follow"),
        Line::from("Esc       back"),
        Line::from("q         quit from the service list"),
        Line::from("Ctrl-C    quit from anywhere"),
        Line::from("?         toggle this help"),
    ];
    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(color::fg(theme, Role::Accent));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Left)
            .style(Style::default()),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn split_line_pads_between_left_and_right() {
        let left = vec![Span::raw("left")];
        let right = vec![Span::raw("right")];
        let line = split_line(20, left, right);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rendered.width(), 20.min(rendered.width()));
        assert!(rendered.starts_with("left"));
        assert!(rendered.ends_with("right"));
    }

    #[test]
    fn split_line_drops_the_right_side_when_it_would_overflow() {
        let left = vec![Span::raw("a very long left side indeed")];
        let right = vec![Span::raw("right")];
        let line = split_line(10, left, right);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!rendered.contains("right"));
    }

    #[test]
    fn truncate_to_width_keeps_short_text_untouched() {
        assert_eq!(truncate_to_width("short", 20), "short");
    }

    #[test]
    fn truncate_to_width_marks_a_cut_with_an_ellipsis() {
        let truncated = truncate_to_width("a rather long participant id here", 10);
        assert!(truncated.width() <= 10);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn truncate_to_width_is_unicode_width_aware_not_byte_aware() {
        // Wide (double-width) CJK characters must count as 2 columns each,
        // not 1 byte/char - a naive `chars().take(n)` would fit twice as
        // many as actually display.
        let text = "宽字符宽字符宽字符";
        let truncated = truncate_to_width(text, 6);
        assert!(
            truncated.width() <= 6,
            "{truncated} exceeds 6 display columns"
        );
    }

    #[test]
    fn simulation_clock_slot_is_empty_outside_simulation_mode() {
        let sample = ClockSample {
            now_ns: 5_000_000_000,
            step: 42,
        };
        assert_eq!(simulation_clock_slot("run", Some(sample)), "");
    }

    #[test]
    fn simulation_clock_slot_is_empty_before_the_first_sample() {
        assert_eq!(simulation_clock_slot("simulation", None), "");
    }

    #[test]
    fn simulation_clock_slot_shows_the_latest_step_and_seconds() {
        let sample = ClockSample {
            now_ns: 5_500_000_000,
            step: 42,
        };
        let text = simulation_clock_slot("simulation", Some(sample));
        assert!(text.contains("step 42"), "{text}");
        assert!(text.contains("5.5s"), "{text}");
    }

    #[test]
    fn host_resource_slot_shows_n_a_when_no_sample_has_arrived() {
        let now = Instant::now();
        assert_eq!(host_resource_slot(None, now), "cpu n/a");
        assert_eq!(host_resource_role(None, now), Role::Muted);
    }

    #[test]
    fn host_resource_slot_renders_cpu_and_ram_once_a_sample_arrives() {
        let now = Instant::now();
        let host = Timestamped {
            value: HostSample {
                cpu_pct: 42.0,
                ram_used_bytes: 4 * 1024 * 1024 * 1024,
                ram_total_bytes: 16 * 1024 * 1024 * 1024,
                load_1m: 0.9,
                window_ns: 1_000_000_000,
            },
            received_at: now,
        };
        let text = host_resource_slot(Some(&host), now);
        assert!(text.contains("cpu 42%"), "{text}");
        assert!(text.contains("ram"), "{text}");
        assert!(text.contains("4.0G"), "{text}");
        assert!(text.contains("16.0G"), "{text}");
        assert!(!text.contains("stale"), "{text}");
    }

    /// The receive-time freshness check itself: a sample older than
    /// `DEFAULT_FRESHNESS_TTL` renders `(stale)` and the muted role, even
    /// though the value is still cached and could otherwise render as live.
    #[test]
    fn host_resource_slot_marks_a_sample_stale_past_the_freshness_ttl() {
        let received_at = Instant::now();
        let now = received_at + DEFAULT_FRESHNESS_TTL + Duration::from_secs(1);
        let host = Timestamped {
            value: HostSample {
                cpu_pct: 10.0,
                ram_used_bytes: 1,
                ram_total_bytes: 2,
                load_1m: 0.1,
                window_ns: 1,
            },
            received_at,
        };
        let text = host_resource_slot(Some(&host), now);
        assert!(text.contains("stale"), "{text}");
        assert_eq!(host_resource_role(Some(&host), now), Role::Muted);
    }

    fn topic(name: &str, producer: &str, rate: f32, count: u64) -> TopicMetric {
        TopicMetric {
            topic: name.to_string(),
            from_participant: producer.to_string(),
            ingress_rate_hz: rate,
            count,
        }
    }

    #[test]
    fn topic_is_stale_below_the_threshold_only() {
        assert!(topic_is_stale(&topic("a", "p", 0.0, 10)));
        assert!(!topic_is_stale(&topic("a", "p", 5.0, 10)));
    }

    #[test]
    fn sort_traffic_rows_by_rate_is_highest_first() {
        let a = topic("a", "p", 1.0, 1);
        let b = topic("b", "p", 9.0, 1);
        let c = topic("c", "p", 3.0, 1);
        let mut rows = vec![&a, &b, &c];
        sort_traffic_rows(&mut rows, TrafficSort::Rate);
        assert_eq!(
            rows.iter()
                .map(|row| row.topic.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn sort_traffic_rows_by_topic_is_alphabetic() {
        let a = topic("zeta", "p", 1.0, 1);
        let b = topic("alpha", "p", 1.0, 1);
        let mut rows = vec![&a, &b];
        sort_traffic_rows(&mut rows, TrafficSort::Topic);
        assert_eq!(
            rows.iter()
                .map(|row| row.topic.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
    }

    #[test]
    fn sort_traffic_rows_by_status_surfaces_stale_topics_first() {
        let live = topic("live", "p", 5.0, 1);
        let stale = topic("stale", "p", 0.0, 1);
        let mut rows = vec![&live, &stale];
        sort_traffic_rows(&mut rows, TrafficSort::Status);
        assert_eq!(rows[0].topic, "stale");
    }

    /// Traffic table renders `stale`/`live` per row and honors the filter
    /// buffer - the acceptance criterion for the Traffic tab's sort/filter
    /// model, exercised through the real `draw_traffic_tab` against a
    /// `TestBackend` rather than re-deriving the row text by hand.
    #[test]
    fn traffic_tab_renders_filtered_rows_with_stale_status() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let now = Instant::now();
        let sample = Timestamped {
            value: RouterMetricsSample {
                topics: vec![
                    topic("v1/drive/target", "mission", 12.0, 500),
                    topic("v2/battery/state", "battery", 0.0, 10),
                ],
                throughput_msg_s: 12.0,
                window_ns: 1_000_000_000,
            },
            received_at: now,
        };
        let mut state = AppState::new();
        state.traffic.filter = "battery".to_string();

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        terminal
            .draw(|frame| {
                draw_traffic_tab(
                    frame,
                    theme,
                    &state,
                    Some(&sample),
                    &RuntimeStore::new(),
                    now,
                    frame.area(),
                )
            })
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("battery"), "{content}");
        assert!(content.contains("stale"), "{content}");
        assert!(!content.contains("drive/target"), "{content}");
    }

    /// Finding A5's Traffic "potential consumers": once `RuntimeStore` knows
    /// a participant declared the topic's contract as an input, the
    /// CONSUMERS column must show a nonzero, locally-known count instead of
    /// being silently absent.
    #[test]
    fn traffic_tab_shows_a_nonzero_consumers_count_from_the_runtime_store() {
        use phoxal::check::ParticipantContractSurface;
        use phoxal::participant::launch::{
            BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
        };
        use phoxal::participant::metadata::ParticipantMetaContract;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use crate::launch_plan::{
            LaunchMode, LaunchPlan, ParticipantExecution, ParticipantLaunchRecord, RobotLaunch,
        };
        use crate::stores::runtime_store::RuntimeStore;

        let now = Instant::now();
        let sample = Timestamped {
            value: RouterMetricsSample {
                topics: vec![topic("v1::drive::Target", "mission", 12.0, 500)],
                throughput_msg_s: 12.0,
                window_ns: 1_000_000_000,
            },
            received_at: now,
        };
        let plan = LaunchPlan {
            mode: LaunchMode::Run,
            site: Vec::new(),
            robots: vec![RobotLaunch {
                id: "rover-01".to_string(),
                namespace: "dev".to_string(),
                participants: vec![ParticipantLaunchRecord {
                    artifact_id: "drive".to_string(),
                    execution: ParticipantExecution::OfficialArtifact {
                        artifact_ref: "phoxal/service-drive@0.4.0".to_string(),
                    },
                    launch: ParticipantLaunch {
                        participant_id: "drive".to_string(),
                        namespace: "dev".to_string(),
                        robot_id: "rover-01".to_string(),
                        bus: BusProfile {
                            connect_endpoints: vec!["tcp/localhost:7447".to_string()],
                        },
                        clock: ClockMode::Real,
                        config: None,
                        robot_root: None,
                        component_instance: None,
                        shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
                    },
                    launch_ownership: LaunchOwnership::CliManaged,
                }],
                substitutions: Vec::new(),
            }],
        };
        let surfaces = vec![ParticipantContractSurface {
            participant_id: "drive".to_string(),
            contracts: vec![ParticipantMetaContract {
                role: "subscribe".to_string(),
                version: "v1".to_string(),
                contract: "drive::Target".to_string(),
                external: false,
            }],
        }];
        let runtime = RuntimeStore::from_launch_plan(&plan, &surfaces);

        let state = AppState::new();
        // Wide enough that the added CONSUMERS column is not truncated away
        // (TOPIC(28) + PRODUCER(16) + RATE(8) + COUNT(9) + STATUS(6) +
        // CONSUMERS(9) + 5 separating spaces needs 81 columns).
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        terminal
            .draw(|frame| {
                draw_traffic_tab(
                    frame,
                    theme,
                    &state,
                    Some(&sample),
                    &runtime,
                    now,
                    frame.area(),
                )
            })
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        let row = content
            .lines()
            .find(|line| line.contains("v1::drive::Target"))
            .expect("the topic row must render");
        assert!(
            row.trim_end().ends_with('1'),
            "the CONSUMERS column must show 1 known consumer: {row:?}"
        );
    }

    /// The joypad Devices tab marks the AUTHORITATIVE `selected` device from
    /// the sample - never a local guess - independent of the cursor
    /// position, which is exactly the "selection updates from the tool's own
    /// next Devices publish" acceptance criterion.
    #[test]
    fn joypad_tab_marks_the_authoritative_selection_not_the_cursor() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let now = Instant::now();
        let sample = Timestamped {
            value: JoypadDevicesSample {
                available: vec![
                    crate::telemetry::JoypadDevice {
                        id: "pad-a".to_string(),
                        name: "Pad A".to_string(),
                        connected: true,
                    },
                    crate::telemetry::JoypadDevice {
                        id: "pad-b".to_string(),
                        name: "Pad B".to_string(),
                        connected: true,
                    },
                ],
                selected: Some("pad-b".to_string()),
                last_error: None,
            },
            received_at: now,
        };
        let mut state = AppState::new();
        state.devices.cursor = 0; // cursor on pad-a, selection is pad-b

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        terminal
            .draw(|frame| draw_joypad_tab(frame, theme, &state, Some(&sample), now, frame.area()))
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        let pad_a_line = content.lines().find(|line| line.contains("Pad A")).unwrap();
        let pad_b_line = content.lines().find(|line| line.contains("Pad B")).unwrap();
        assert!(!pad_a_line.contains('✓'), "{pad_a_line}");
        assert!(pad_b_line.contains('✓'), "{pad_b_line}");
    }

    #[test]
    fn footer_collapses_to_filter_hint_while_filtering() {
        let mut state = AppState::new();
        state.navigator.filtering = true;
        let segments = footer_segments(&state);
        assert!(segments.iter().any(|segment| segment.contains("filter")));
        assert!(!segments.iter().any(|segment| segment.contains("restart")));
    }

    #[test]
    fn participant_row_text_truncates_a_wide_unicode_id_to_the_given_width() {
        let status = ParticipantStatus::new(
            "宽字符宽字符宽字符",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Ready,
        );
        let text = participant_row_text(&status, 8);
        assert!(
            text.width() <= 8,
            "{text} exceeds the requested 8-column budget"
        );
    }

    /// Design doc part 7: the navigator row must carry both the state
    /// SYMBOL and its TEXT label, not the symbol alone.
    #[test]
    fn participant_row_text_includes_both_the_symbol_and_the_text_label() {
        let status = ParticipantStatus::new(
            "drive",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Degraded,
        );
        let text = participant_row_text(&status, 40);
        assert!(
            text.contains(state_symbol(ParticipantState::Degraded)),
            "{text}"
        );
        assert!(text.contains("degraded"), "{text}");
    }

    #[test]
    fn participant_row_text_shows_the_restart_count_only_when_nonzero() {
        let mut status = ParticipantStatus::new(
            "drive",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Ready,
        );
        assert!(!participant_row_text(&status, 40).contains('↻'));
        status.restart_count = 3;
        assert!(participant_row_text(&status, 40).contains("↻3"));
    }

    fn sample_board() -> BoardSnapshot {
        let mut board = BoardSnapshot::default();
        for (id, kind, state) in [
            (
                "tool-router",
                crate::participant_kind::ParticipantKind::Tool,
                ParticipantState::Ready,
            ),
            (
                "drive",
                crate::participant_kind::ParticipantKind::Service,
                ParticipantState::Ready,
            ),
            (
                "left_wheel",
                crate::participant_kind::ParticipantKind::Driver,
                ParticipantState::Failed,
            ),
        ] {
            board
                .participants
                .insert(id.to_string(), ParticipantStatus::new(id, kind, state));
        }
        board
    }

    fn sample_title() -> TitleInfo {
        TitleInfo {
            robot: "rover-01".to_string(),
            channel: "dev".to_string(),
            mode: "run",
        }
    }

    /// Fields the Overview panel shows when [`RuntimeStore`] has no metadata
    /// for this participant - the fallback case, not the normal one now that
    /// finding A5's sidecar exists. Must render the explicit unknown marker
    /// rather than a fabricated value.
    #[test]
    fn overview_shows_unknown_for_fields_not_wired_to_the_board() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let status = ParticipantStatus::new(
            "drive",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Ready,
        );
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let now = Instant::now();
        terminal
            .draw(|frame| {
                draw_runtime_overview(
                    frame,
                    theme,
                    &status,
                    None,
                    None,
                    None,
                    None,
                    now,
                    0,
                    frame.area(),
                )
            })
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("artifact"), "{content}");
        assert!(content.contains("input"), "{content}");
        assert!(content.contains("output"), "{content}");
        assert!(content.contains("ownership"), "{content}");
        assert!(content.contains("time to ready"), "{content}");
        assert!(
            content.matches("unknown").count() >= 5,
            "every field with no runtime metadata must show the unknown marker: {content}"
        );
        // Process telemetry absence is explicit rather than a fabricated zero.
        let cpu_line = content
            .lines()
            .find(|line| line.trim_start().starts_with("cpu"))
            .expect("a cpu row must render");
        assert_eq!(cpu_line.trim_end(), "cpu       n/a", "{cpu_line:?}");
    }

    /// Finding A5's positive case: once `RuntimeStore` has this
    /// participant's launch-time metadata, the Overview panel renders the
    /// REAL artifact reference, split input/output contracts, ownership, and
    /// time-to-ready - never the unknown marker.
    #[test]
    fn overview_shows_populated_runtime_metadata_fields() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let status = ParticipantStatus::new(
            "drive",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Ready,
        );
        let metadata = RuntimeParticipantMetadata {
            artifact_ref: Some("phoxal/service-drive@0.4.0".to_string()),
            ownership: crate::stores::runtime_store::RuntimeOwnership::CliManaged,
            input_contracts: vec!["v1::drive::Target".to_string()],
            output_contracts: vec!["v1::drive::State".to_string()],
        };
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let now = Instant::now();
        terminal
            .draw(|frame| {
                draw_runtime_overview(
                    frame,
                    theme,
                    &status,
                    None,
                    Some(&metadata),
                    Some(Duration::from_millis(1500)),
                    None,
                    now,
                    0,
                    frame.area(),
                )
            })
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("phoxal/service-drive@0.4.0"), "{content}");
        assert!(content.contains("v1::drive::Target"), "{content}");
        assert!(content.contains("v1::drive::State"), "{content}");
        assert!(content.contains("cli-managed"), "{content}");
        assert!(content.contains("1.5s"), "{content}");
        assert!(
            !content.contains("unknown"),
            "populated metadata must never fall back to the unknown marker: {content}"
        );
    }

    /// Launch mechanics are intentionally absent from the operator Overview.
    #[test]
    fn overview_hides_the_launch_command_and_its_environment() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut status = ParticipantStatus::new(
            "drive",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Ready,
        );
        let mut env = std::collections::BTreeMap::new();
        env.insert("PHOXAL_ROBOT_ID".to_string(), "rover-01".to_string());
        status.launch_command = Some(crate::supervisor::ParticipantLaunchCommand {
            command_line: "service-drive --config /etc/drive.yaml".to_string(),
            env,
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let now = Instant::now();
        terminal
            .draw(|frame| {
                draw_runtime_overview(
                    frame,
                    theme,
                    &status,
                    None,
                    None,
                    None,
                    None,
                    now,
                    0,
                    frame.area(),
                )
            })
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        assert!(!content.contains("service-drive"), "{content}");
        assert!(!content.contains("environment"), "{content}");
        assert!(!content.contains("PHOXAL_ROBOT_ID"), "{content}");
    }

    /// A live `Process` sample renders its value; a stale one (past
    /// `DEFAULT_FRESHNESS_TTL`) is marked `(stale)` rather than shown as if
    /// it were still current.
    #[test]
    fn overview_marks_a_stale_process_sample() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let status = ParticipantStatus::new(
            "drive",
            crate::participant_kind::ParticipantKind::Service,
            ParticipantState::Ready,
        );
        let received_at = Instant::now();
        let now = received_at + DEFAULT_FRESHNESS_TTL + Duration::from_secs(1);
        let process = Timestamped {
            value: ProcessSample {
                cpu_pct: 7.0,
                rss_bytes: 20 * 1024 * 1024,
                window_ns: 1,
            },
            received_at,
        };
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        terminal
            .draw(|frame| {
                draw_runtime_overview(
                    frame,
                    theme,
                    &status,
                    Some(&process),
                    None,
                    None,
                    None,
                    now,
                    0,
                    frame.area(),
                )
            })
            .expect("draw must not fail");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("7.0%"), "{content}");
        assert!(content.contains("(stale)"), "{content}");
    }

    /// A full frame at a very ordinary size must render without panicking,
    /// and every top-level chrome element (title, group headings, footer)
    /// must actually land somewhere in the buffer.
    #[test]
    fn draws_a_full_frame_at_80x24_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();
        let board = sample_board();
        let mut logs = LogStore::new();
        let mut state = AppState::new();
        state.sync(&board);

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    theme,
                    &title,
                    &board,
                    &mut logs,
                    &state,
                    &TelemetrySnapshot::default(),
                    &RuntimeStore::new(),
                    Instant::now(),
                    &DiagnosticsStore::default(),
                )
            })
            .expect("draw must not fail at 80x24");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("phoxal"), "title bar missing: {content}");
        assert!(
            content.contains("System"),
            "System group heading missing: {content}"
        );
        assert!(content.contains("quit"), "footer hint missing: {content}");
    }

    #[test]
    fn diagnostics_tab_renders_structured_rows_and_header_badge() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();
        let board = sample_board();
        let mut logs = LogStore::new();
        let mut state = AppState::new();
        state.sync(&board);
        state.open_diagnostics();
        let mut diagnostics = DiagnosticsStore::default();
        diagnostics.record(
            Duration::from_millis(1250),
            crate::session::event::DiagnosticSource::Dependency,
            DiagnosticLevel::Warn,
            "connection retrying".to_string(),
        );

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    theme,
                    &title,
                    &board,
                    &mut logs,
                    &state,
                    &TelemetrySnapshot::default(),
                    &RuntimeStore::new(),
                    Instant::now(),
                    &diagnostics,
                )
            })
            .expect("draw must not fail");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("diagnostics 1"), "{content}");
        assert!(content.contains("Diagnostics (1)"), "{content}");
        assert!(content.contains("WARN"), "{content}");
        assert!(content.contains("dependency"), "{content}");
        assert!(content.contains("connection retrying"), "{content}");
    }

    /// A pathologically narrow terminal (design doc: "a very narrow width")
    /// must degrade gracefully - no panic, no underflow in the width-based
    /// layout math (`nav_width` clamping, `split_line`'s saturating
    /// subtraction, `truncate_to_width`).
    #[test]
    fn draws_without_panicking_at_a_very_narrow_width() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for (width, height) in [(20, 10), (8, 6), (1, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let theme = Theme::new(crate::theme::ColorCapability::None);
            let title = sample_title();
            let board = sample_board();
            let mut logs = LogStore::new();
            let mut state = AppState::new();
            state.sync(&board);

            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        theme,
                        &title,
                        &board,
                        &mut logs,
                        &state,
                        &TelemetrySnapshot::default(),
                        &RuntimeStore::new(),
                        Instant::now(),
                        &DiagnosticsStore::default(),
                    )
                })
                .unwrap_or_else(|error| panic!("draw must not fail at {width}x{height}: {error}"));
        }
    }

    /// A resize mid-session (the operator's terminal window changing size)
    /// must not panic on the next redraw - `ratatui::Terminal::resize`
    /// mirrors what a real `crossterm::event::Event::Resize` drives.
    #[test]
    fn survives_a_resize_event_between_two_redraws() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();
        let board = sample_board();
        let mut logs = LogStore::new();
        let mut state = AppState::new();
        state.sync(&board);

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    theme,
                    &title,
                    &board,
                    &mut logs,
                    &state,
                    &TelemetrySnapshot::default(),
                    &RuntimeStore::new(),
                    Instant::now(),
                    &DiagnosticsStore::default(),
                )
            })
            .expect("first draw must succeed");

        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 40, 12))
            .expect("resize must succeed");

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    theme,
                    &title,
                    &board,
                    &mut logs,
                    &state,
                    &TelemetrySnapshot::default(),
                    &RuntimeStore::new(),
                    Instant::now(),
                    &DiagnosticsStore::default(),
                )
            })
            .expect("draw after resize must succeed");
    }

    #[test]
    fn switching_to_a_shorter_tab_clears_the_previous_tab_contents() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();
        let mut board = BoardSnapshot::default();
        board.participants.insert(
            "drive".to_string(),
            ParticipantStatus::new(
                "drive",
                crate::participant_kind::ParticipantKind::Service,
                ParticipantState::Ready,
            ),
        );
        let mut logs = LogStore::new();
        logs.record(crate::supervisor::RoutedLogLine {
            participant: "drive".to_string(),
            source: crate::supervisor::LogSource::Raw,
            text: "OLD_LONG_LOG_CONTENT_THAT_MUST_DISAPPEAR".to_string(),
        });
        let mut state = AppState::new();
        state.sync(&board);
        state.view = View::Runtime("drive".to_string());
        state.focus = Focus::Detail;
        state.panel = Panel::Logs;

        let mut draw_frame = |terminal: &mut Terminal<TestBackend>, state: &AppState| {
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        theme,
                        &title,
                        &board,
                        &mut logs,
                        state,
                        &TelemetrySnapshot::default(),
                        &RuntimeStore::new(),
                        Instant::now(),
                        &DiagnosticsStore::default(),
                    )
                })
                .expect("draw must succeed");
        };

        draw_frame(&mut terminal, &state);
        assert!(
            buffer_text(terminal.backend().buffer()).contains("OLD_LONG_LOG_CONTENT"),
            "the first frame must contain the log marker"
        );

        state.panel = Panel::Overview;
        draw_frame(&mut terminal, &state);
        let content = buffer_text(terminal.backend().buffer());
        assert!(!content.contains("OLD_LONG_LOG_CONTENT"), "{content}");
    }

    /// Every `ColorCapability` tier (including the two non-truecolor
    /// fallbacks) must render a full frame without panicking - the TUI reuses
    /// `theme`'s own degradation (`crate::tui::color`), so this is the
    /// integration proof that the degradation actually reaches every widget.
    #[test]
    fn draws_without_panicking_across_every_color_capability() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for capability in [
            crate::theme::ColorCapability::TrueColor,
            crate::theme::ColorCapability::Ansi256,
            crate::theme::ColorCapability::Ansi16,
            crate::theme::ColorCapability::None,
        ] {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let theme = Theme::new(capability);
            let title = sample_title();
            let board = sample_board();
            let mut logs = LogStore::new();
            let mut state = AppState::new();
            state.sync(&board);

            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        theme,
                        &title,
                        &board,
                        &mut logs,
                        &state,
                        &TelemetrySnapshot::default(),
                        &RuntimeStore::new(),
                        Instant::now(),
                        &DiagnosticsStore::default(),
                    )
                })
                .unwrap_or_else(|error| panic!("draw must not fail under {capability:?}: {error}"));
        }
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn sample_identity() -> IdentitySummary {
        IdentitySummary {
            robot: "rover-01".to_string(),
            channel: "dev".to_string(),
            manifest: "./robot.yaml".to_string(),
            terminal: crate::identity::TerminalMode::Full,
        }
    }

    /// The startup surface shows identity and the active phase without
    /// printing diagnostics into the terminal frame.
    #[test]
    fn draws_the_startup_surface_with_card_and_phase() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();
        let identity = sample_identity();

        let mut startup = StartupState::new();
        startup.apply_event(&crate::session::event::SessionEvent::PhaseStarted {
            id: crate::session::event::PhaseId::new("download"),
            label: "Downloading artifacts".to_string(),
        });
        terminal
            .draw(|frame| draw_startup(frame, theme, &title, Some(&identity), &startup))
            .expect("draw_startup must not fail at 80x24");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("phoxal-cli"), "{content}");
        assert!(content.contains("rover-01"), "{content}");
        assert!(content.contains("Downloading artifacts"), "{content}");
        assert!(
            content.contains("preparing"),
            "state label missing: {content}"
        );
    }

    /// A finished phase shows its compact completion result (a checkmark and
    /// elapsed time), not the running form.
    #[test]
    fn draws_a_finished_phase_as_a_compact_completion_line() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();

        let mut startup = StartupState::new();
        startup.apply_event(&crate::session::event::SessionEvent::PhaseStarted {
            id: crate::session::event::PhaseId::new("download"),
            label: "Downloading artifacts".to_string(),
        });
        startup.apply_event(&crate::session::event::SessionEvent::PhaseFinished {
            id: crate::session::event::PhaseId::new("download"),
            outcome: crate::session::event::PhaseOutcome::Succeeded,
            elapsed: std::time::Duration::from_millis(1500),
        });

        terminal
            .draw(|frame| draw_startup(frame, theme, &title, None, &startup))
            .expect("draw_startup must not fail");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Downloading artifacts"), "{content}");
        assert!(content.contains("1.5s"), "{content}");
    }

    /// A running phase with progress must show the counter AND the detail
    /// text carried by `PhaseProgress`, not just the bare label.
    #[test]
    fn draws_a_running_phase_with_progress_counter_and_detail() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();

        let mut startup = StartupState::new();
        startup.apply_event(&crate::session::event::SessionEvent::PhaseStarted {
            id: crate::session::event::PhaseId::new("download"),
            label: "Downloading artifacts".to_string(),
        });
        startup.apply_event(&crate::session::event::SessionEvent::PhaseProgress {
            id: crate::session::event::PhaseId::new("download"),
            completed: 2,
            total: 5,
            detail: Some("phoxal/service-drive".to_string()),
        });

        terminal
            .draw(|frame| draw_startup(frame, theme, &title, None, &startup))
            .expect("draw_startup must not fail");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("(2/5)"), "{content}");
        assert!(content.contains("phoxal/service-drive"), "{content}");
    }

    /// No `robot.yaml` discoverable (`identity: None`) must still render a
    /// bare one-line card rather than an empty gap or a panic.
    #[test]
    fn draws_a_bare_version_line_when_no_identity_is_discoverable() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let title = sample_title();
        let startup = StartupState::new();

        terminal
            .draw(|frame| draw_startup(frame, theme, &title, None, &startup))
            .expect("draw_startup must not fail without identity");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains(env!("CARGO_PKG_VERSION")), "{content}");
    }

    /// The startup surface must degrade without panicking at a pathologically
    /// narrow size, mirroring the navigator's own narrow-width proof.
    #[test]
    fn draws_the_startup_surface_without_panicking_at_a_very_narrow_width() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let identity = sample_identity();
        for (width, height) in [(20, 10), (8, 6), (1, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let theme = Theme::new(crate::theme::ColorCapability::None);
            let title = sample_title();
            let startup = StartupState::new();

            terminal
                .draw(|frame| draw_startup(frame, theme, &title, Some(&identity), &startup))
                .unwrap_or_else(|error| {
                    panic!("draw_startup must not fail at {width}x{height}: {error}")
                });
        }
    }
}
