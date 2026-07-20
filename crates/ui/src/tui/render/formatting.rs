//! Formatting rendering responsibilities.

use super::*;

pub(super) fn severity_label(severity: LogSeverity) -> &'static str {
    match severity {
        LogSeverity::Trace => "TRACE",
        LogSeverity::Debug => "DEBUG",
        LogSeverity::Info => "INFO",
        LogSeverity::Warn => "WARN",
        LogSeverity::Error => "ERROR",
    }
}

pub(super) fn empty_as_all(value: &str) -> &str {
    if value.is_empty() { "all" } else { value }
}

pub(super) fn ellipsize(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let budget = width - 1;
    let mut used: usize = 0;
    let mut shortened = String::new();
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > budget {
            break;
        }
        shortened.push(character);
        used = used.saturating_add(character_width);
    }
    shortened.push('…');
    shortened
}

pub(super) fn sanitize_and_ellipsize(text: &str, width: usize) -> String {
    ellipsize(&sanitize_terminal_text(text), width)
}

pub(super) fn fit_cell(text: &str, width: usize) -> String {
    let mut fitted = ellipsize(text, width);
    fitted.push_str(&" ".repeat(width.saturating_sub(UnicodeWidthStr::width(fitted.as_str()))));
    fitted
}

pub(super) fn sanitize_and_fit_cell(text: &str, width: usize) -> String {
    fit_cell(&sanitize_terminal_text(text), width)
}

pub(super) fn shell_block<'a>(theme: Theme, title: &'a str) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(color::fg(theme, Role::Border))
}

pub(super) fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

pub(super) fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width.min(area.width),
        height.min(area.height),
    )
}
