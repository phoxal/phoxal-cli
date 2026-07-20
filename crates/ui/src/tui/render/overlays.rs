//! Overlays rendering responsibilities.

use super::*;

pub(super) fn draw_footer(frame: &mut Frame, theme: Theme, state: &AppState, area: Rect) {
    let page_help = if state.navigation == NavigationLevel::Tabs {
        "←→ choose page · Enter open"
    } else {
        match state.page {
            Page::Overview => "Esc tabs",
            Page::Runtimes if state.runtime_detail_id.is_some() => {
                "Esc runtime list · l logs · r restart"
            }
            Page::Runtimes => "↑↓ choose runtime · Enter details · l logs · r restart · Esc tabs",
            Page::Logs => "←→ filters · Enter edit/change · ↑↓ scroll · Space pause · Esc tabs",
            Page::Bus => "←→ controls · Enter edit/change · ↑↓ scroll · Esc tabs",
            Page::Input => {
                "↑↓ choose device · Enter select · e enable · x disable · r rescan · Esc tabs"
            }
        }
    };
    let global_help = if area.width >= 104 {
        "i session info · ? help · q quit"
    } else if area.width >= 68 {
        "i info · ? help · q quit"
    } else {
        "? help · q quit"
    };
    let global_width = u16::try_from(global_help.chars().count())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(global_width)])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(" {page_help}")).style(color::muted(theme)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(global_help)
            .alignment(Alignment::Right)
            .style(color::muted(theme)),
        columns[1],
    );
}

pub(super) fn draw_help(frame: &mut Frame, theme: Theme, area: Rect) {
    let lines = vec![
        Line::from("Arrows       move the soft cursor"),
        Line::from("Enter        open / activate"),
        Line::from("Esc          back one level"),
        Line::from("1-5          open a page directly"),
        Line::from("i            session information"),
        Line::from("? / Esc      close help"),
        Line::from("q / Ctrl-C   stop session"),
        Line::from(""),
        Line::from("More information"),
        Line::from("https://phoxal.com"),
        Line::from("Open an issue"),
        Line::from("github.com/phoxal/phoxal-cli/issues"),
    ];
    let help_height = if area.width < 70 { 17 } else { 15 };
    let target = centered_fixed(area, area.width.min(70), area.height.min(help_height));
    frame.render_widget(Clear, target);
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Help"))
            .wrap(Wrap { trim: true }),
        target,
    );
}

pub(super) fn draw_session_info(
    frame: &mut Frame,
    theme: Theme,
    title: &TitleInfo,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let manifest = sanitize_terminal_text(&title.manifest);
    let started = title.started_at.duration_since(UNIX_EPOCH).map_or_else(
        |_| "n/a".to_string(),
        |value| format!("unix {}", value.as_secs()),
    );
    let lines = vec![
        Line::from(format!(
            "robot            {}",
            sanitize_terminal_text(&title.robot)
        )),
        Line::from(format!(
            "namespace        {}",
            sanitize_terminal_text(&title.namespace)
        )),
        Line::from(format!("mode             {}", title.mode)),
        Line::from(format!("manifest         {manifest}")),
        Line::from(format!(
            "artifact channel {}",
            sanitize_terminal_text(&title.channel)
        )),
        Line::from(format!(
            "bus endpoint     {}",
            sanitize_terminal_text(&title.bus_endpoint)
        )),
        Line::from(format!("CLI              {}", env!("CARGO_PKG_VERSION"))),
        Line::from(format!(
            "start time       {started} · {} ago",
            human::duration(model.now.saturating_duration_since(title.started_instant))
        )),
    ];
    let info_height = if area.width < 70 { 15 } else { 13 };
    let target = centered_fixed(area, area.width.min(74), area.height.min(info_height));
    frame.render_widget(Clear, target);
    frame.render_widget(
        Paragraph::new(lines)
            .block(shell_block(theme, "Session Information · i/Esc close"))
            .wrap(Wrap { trim: true }),
        target,
    );
}
