//! Bounded log-routing records shared by session adapters and presentation.

use std::time::SystemTime;

pub const MAX_ROUTED_LOG_TEXT_CHARS: usize = 4_096;

#[must_use]
pub fn bounded_log_text(text: &str) -> String {
    let mut chars = text.chars();
    let mut bounded = chars
        .by_ref()
        .take(MAX_ROUTED_LOG_TEXT_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

/// Strip terminal escape and non-printing format controls at remote-data
/// ingress and again at presentation boundaries.
#[must_use]
pub fn sanitize_terminal_text(text: &str) -> String {
    strip_ansi(text)
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

fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            output.push(character);
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
    output
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

/// Where a routed log line came from; consumers deduplicate on this routing
/// identity rather than comparing rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    /// A structured `logs/{participant_id}` bus event
    /// A structured bus event: the primary source once a participant can
    /// publish on the bus.
    Bus,
    /// A captured stdout/stderr line from the supervised child process
    /// Captured child stdout/stderr: the source before bus connectivity.
    Raw,
}

/// Severity retained with routed logs so the global Logs page can filter
/// structured events without parsing their rendered text. Raw child output
/// has no typed level and is conservatively recorded as Info.
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

/// One routed log line, separate from the persisted board's short history so
/// presentation can maintain its own bounded scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedLogLine {
    pub participant: String,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub text: String,
    /// Producer event time for retained records, receive time for local raw
    /// and diagnostic records. Presentation merges both bounded sources by
    /// this timestamp without rewriting retained history during a snapshot.
    pub event_time: SystemTime,
    /// Present for robot-owned retained records; local raw/diagnostic lines
    /// remain unscoped.
    pub scope: Option<LogScope>,
}

/// One replacement from the client-owned retention store to presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedLogUpdate {
    ReplaceAll(Vec<RoutedLogLine>),
}

#[cfg(test)]
mod tests {
    use super::sanitize_terminal_text;

    #[test]
    fn terminal_text_strips_ansi_and_nonprinting_unicode_controls() {
        assert_eq!(
            sanitize_terminal_text("left\u{1b}[2Jmid\u{202e}right\u{e0001}"),
            "leftmid right "
        );
    }
}
