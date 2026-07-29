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

/// Strip terminal escape sequences and non-printing format controls.
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
            if character.is_control() || is_terminal_format_control(character) {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn is_terminal_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{13455}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogScope {
    pub namespace: String,
    pub robot_id: String,
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
    pub scope: Option<LogScope>,
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
    pub scope: Option<LogScope>,
}

pub type LogRead = ObservationQuery<LogQuery>;
pub type LogWindow = ObservationWindow<LogRow>;

#[cfg(test)]
mod tests {
    use super::sanitize_terminal_text;

    #[test]
    fn terminal_text_strips_ansi_and_nonprinting_unicode_controls() {
        assert_eq!(
            sanitize_terminal_text("safe\u{1b}[31m red\u{1b}[0m\u{202e}tail"),
            "safe red tail"
        );
    }

    #[test]
    fn osc_string_terminator_preserves_following_text() {
        assert_eq!(sanitize_terminal_text("a\u{1b}]0;t\u{1b}\\b"), "ab");
    }
}
