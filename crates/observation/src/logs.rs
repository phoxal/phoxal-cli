use std::time::SystemTime;

use crate::{ObservationQuery, ObservationWindow};

pub const MAX_LOG_TEXT_CHARS: usize = 4_096;

#[must_use]
pub fn bounded_log_text(text: &str) -> String {
    let mut chars = text.chars();
    let mut bounded = chars.by_ref().take(MAX_LOG_TEXT_CHARS).collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

/// Strip terminal escape sequences, so log text a participant emitted cannot
/// repaint or reposition the terminal it is displayed in.
#[must_use]
pub fn sanitize_terminal_text(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            plain.push(character);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut escaped = false;
                for next in chars.by_ref() {
                    if next == '\u{7}' || (escaped && next == '\\') {
                        break;
                    }
                    escaped = next == '\u{1b}';
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    plain
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Bus,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogSeverity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindowDirection {
    Forward,
    #[default]
    Backward,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilters {
    pub participant: Option<String>,
    pub minimum_severity: Option<LogSeverity>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogAnchor {
    Before(SystemTime),
    After(SystemTime),
    #[default]
    Latest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    pub filters: LogFilters,
    pub anchor: LogAnchor,
    pub direction: WindowDirection,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
    pub event_time: SystemTime,
}

pub type LogRead = ObservationQuery<LogQuery>;
pub type LogWindow = ObservationWindow<LogRow>;

#[cfg(test)]
mod tests {
    use super::sanitize_terminal_text;

    #[test]
    fn terminal_text_strips_ansi_escape_sequences() {
        assert_eq!(
            sanitize_terminal_text("safe\u{1b}[31m red\u{1b}[0m tail"),
            "safe red tail"
        );
    }

    #[test]
    fn osc_string_terminator_preserves_following_text() {
        assert_eq!(sanitize_terminal_text("a\u{1b}]0;t\u{1b}\\b"), "ab");
    }
}
