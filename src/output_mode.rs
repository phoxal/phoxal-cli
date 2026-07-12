//! The output-mode matrix: what stdout/stderr are allowed to carry for a
//! given TTY-ness, `--plain`, `--quiet`, and `--message-format`.
//!
//! |            | stdout                 | stderr                          |
//! |------------|-------------------------|---------------------------------|
//! | TTY        | machine results only    | rich (theme/progress)           |
//! | non-TTY    | machine results only    | append-only line logger         |
//! | `--plain`  | machine results only    | append-only line logger         |
//! | `--json`   | the JSON document only  | nothing                         |
//!
//! stdout never changes shape here - it always carries only the machine
//! result the verb produces (a launch report, a check result, ...); this
//! module only decides how much decoration stderr gets. [`OutputMode`] is
//! computed once in [`crate::commands::dispatch`] and threaded to
//! [`crate::progress`]; the identity/welcome gate
//! ([`crate::identity::IdentityPolicy`]) is computed independently, since its
//! suppression rule does not include `--plain` (see its doc comment).

use crate::commands::MessageFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Interactive stderr: the progress primitive may draw a spinner/bar and
    /// redraw in place.
    Rich,
    /// stderr is append-only: one line per event, no redraw, no cursor
    /// control. Selected by a non-TTY stderr, `--plain`, or `--quiet`.
    Plain,
    /// `--message-format json`: stdout carries only the JSON document and
    /// stderr carries nothing - no identity, no welcome, no progress, no log
    /// lines.
    Json,
}

impl OutputMode {
    #[must_use]
    pub fn compute(is_tty: bool, plain: bool, quiet: bool, message_format: MessageFormat) -> Self {
        if message_format == MessageFormat::Json {
            return Self::Json;
        }
        if !is_tty || plain || quiet {
            return Self::Plain;
        }
        Self::Rich
    }

    /// Whether the progress primitive may draw/redraw (a spinner tick, a
    /// growing bar). `false` for [`Self::Plain`] (single append-only lines
    /// instead) and [`Self::Json`] (nothing at all).
    #[must_use]
    pub const fn allows_progress_drawing(self) -> bool {
        matches!(self, Self::Rich)
    }

    /// Whether stderr may carry any decoration at all (rich or the
    /// append-only fallback). `false` only for [`Self::Json`].
    #[must_use]
    pub const fn allows_stderr_decoration(self) -> bool {
        matches!(self, Self::Rich | Self::Plain)
    }

    #[must_use]
    pub const fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tty_human_is_rich() {
        assert_eq!(
            OutputMode::compute(true, false, false, MessageFormat::Human),
            OutputMode::Rich
        );
    }

    #[test]
    fn non_tty_human_is_plain() {
        assert_eq!(
            OutputMode::compute(false, false, false, MessageFormat::Human),
            OutputMode::Plain
        );
    }

    #[test]
    fn plain_flag_forces_plain_even_on_a_tty() {
        assert_eq!(
            OutputMode::compute(true, true, false, MessageFormat::Human),
            OutputMode::Plain
        );
    }

    #[test]
    fn quiet_flag_forces_plain_even_on_a_tty() {
        assert_eq!(
            OutputMode::compute(true, false, true, MessageFormat::Human),
            OutputMode::Plain
        );
    }

    #[test]
    fn json_wins_over_every_other_input() {
        for is_tty in [true, false] {
            for plain in [true, false] {
                for quiet in [true, false] {
                    assert_eq!(
                        OutputMode::compute(is_tty, plain, quiet, MessageFormat::Json),
                        OutputMode::Json
                    );
                }
            }
        }
    }

    #[test]
    fn only_rich_allows_progress_drawing() {
        assert!(OutputMode::Rich.allows_progress_drawing());
        assert!(!OutputMode::Plain.allows_progress_drawing());
        assert!(!OutputMode::Json.allows_progress_drawing());
    }

    #[test]
    fn json_is_the_only_mode_with_no_stderr_decoration_at_all() {
        assert!(OutputMode::Rich.allows_stderr_decoration());
        assert!(OutputMode::Plain.allows_stderr_decoration());
        assert!(!OutputMode::Json.allows_stderr_decoration());
    }
}
