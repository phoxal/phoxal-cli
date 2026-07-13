//! The explicit output contract for one session (`run`/`simulation run`):
//! which renderer applies, the theme it draws with, and whether the
//! invocation asked for `--quiet`.
//!
//! Replaces scattered `progress::current_mode()`/`Theme::detect_stderr()`
//! calls at the session entry point with one immutable value, built ONCE in
//! [`crate::commands::dispatch`] from the same inputs
//! [`crate::output_mode::OutputMode::compute`] uses, and threaded into
//! [`super::controller::SessionController`]. `crate::progress`'s own
//! process-global mode cell is untouched by this - every non-session verb
//! (`check`, `deploy`, `status`, ...) still asks it directly, and stays doing
//! so until a later wave (see the crate's follow-up plan, Wave D).

use std::time::Duration;

use crate::commands::MessageFormat;
use crate::output_mode::OutputMode;
use crate::theme::Theme;

/// An effectively-unbounded readiness/stage-wait budget for an interactive
/// session (Product decision 6: "no unconditional 60-second teardown in an
/// interactive session"). Not literally `Duration::MAX` - `Instant + Duration`
/// panics on overflow, and every caller of
/// [`OutputContext::wait_budget`] adds this to `Instant::now()` to compute a
/// deadline - but long enough that no real session ever reaches it; a
/// missing clock/participant is a named `waiting`/`degraded` console state
/// for as long as the operator leaves the session open, not a kill.
const NO_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// The immutable output contract for one `run`/`simulation run` invocation.
#[derive(Debug, Clone, Copy)]
pub struct OutputContext {
    pub mode: OutputMode,
    pub theme: Theme,
    pub quiet: bool,
}

impl OutputContext {
    #[must_use]
    pub const fn new(mode: OutputMode, theme: Theme, quiet: bool) -> Self {
        Self { mode, theme, quiet }
    }

    /// The wait budget for an interactive-session readiness/stage wait
    /// (Product decision 6). [`OutputMode::Rich`]/[`OutputMode::Plain`] (a
    /// human at a terminal, or a human-oriented non-TTY stream) get
    /// [`NO_TIMEOUT`] - a missing clock/participant becomes a named
    /// `waiting`/`degraded` console state, never an automatic teardown.
    /// [`OutputMode::Json`] (a machine/batch invocation with no human
    /// watching a "waiting" console) keeps `bounded` so a script still gets a
    /// deterministic failure instead of hanging forever. Any future headless
    /// bounded-wait policy should be an explicit, separate opt-in - never
    /// implicitly shared with the interactive session the way the old fixed
    /// 60s constant was.
    #[must_use]
    pub const fn wait_budget(self, bounded: Duration) -> Duration {
        if self.mode.is_json() {
            bounded
        } else {
            NO_TIMEOUT
        }
    }

    /// Build from the same inputs [`OutputMode::compute`] uses, plus the
    /// theme detected off the same stream. Called once in
    /// [`crate::commands::dispatch`]; every other caller (a unit test, or any
    /// code running outside `dispatch`) should prefer [`Self::new`] with an
    /// explicit mode instead of re-deriving from the live environment, so
    /// tests stay deterministic.
    #[must_use]
    pub fn compute(is_tty: bool, plain: bool, quiet: bool, message_format: MessageFormat) -> Self {
        let mode = OutputMode::compute(is_tty, plain, quiet, message_format);
        Self::new(mode, Theme::detect_stderr(), quiet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorCapability;

    #[test]
    fn compute_matches_output_mode_compute() {
        let ctx = OutputContext::compute(true, false, false, MessageFormat::Human);
        assert_eq!(ctx.mode, OutputMode::Rich);
        assert!(!ctx.quiet);

        let ctx = OutputContext::compute(false, false, false, MessageFormat::Human);
        assert_eq!(ctx.mode, OutputMode::Plain);

        let ctx = OutputContext::compute(true, false, false, MessageFormat::Json);
        assert_eq!(ctx.mode, OutputMode::Json);
    }

    #[test]
    fn new_is_a_plain_immutable_constructor() {
        let ctx = OutputContext::new(OutputMode::Plain, Theme::new(ColorCapability::None), true);
        assert_eq!(ctx.mode, OutputMode::Plain);
        assert!(ctx.quiet);
    }

    /// Product decision 6: only `Json` keeps a bounded wait; `Rich`/`Plain`
    /// (an interactive or human-oriented session) must not be given the same
    /// finite budget that would tear an operator's console down after 60s.
    #[test]
    fn wait_budget_is_bounded_only_for_json() {
        let bounded = Duration::from_secs(60);
        let rich = OutputContext::new(OutputMode::Rich, Theme::new(ColorCapability::None), false);
        let plain = OutputContext::new(OutputMode::Plain, Theme::new(ColorCapability::None), false);
        let json = OutputContext::new(OutputMode::Json, Theme::new(ColorCapability::None), false);

        assert_eq!(json.wait_budget(bounded), bounded);
        assert!(rich.wait_budget(bounded) > bounded);
        assert!(plain.wait_budget(bounded) > bounded);
    }
}
