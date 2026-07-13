//! The explicit output contract for one session (`run`/`simulation run`):
//! which renderer applies, the theme it draws with, and whether the
//! invocation asked for `--quiet`.
//!
//! Built ONCE in [`crate::commands::dispatch`] from the same inputs
//! [`crate::output_mode::OutputMode::compute`] uses, and threaded explicitly
//! into [`super::controller::SessionController`], `AppContext::ui`, and every
//! other mode-aware helper (`crate::progress`, catalog/artifact fetches, git
//! ref resolution) from there - there is no process-global mode cell
//! anywhere in the crate.

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
    /// (Product decision 6). Only [`OutputMode::Rich`] - a true interactive
    /// TTY console that actually renders a live "waiting" state - gets
    /// [`NO_TIMEOUT`]: a missing clock/participant becomes a named
    /// `waiting`/`degraded` console state, never an automatic teardown,
    /// because an operator is watching it. [`OutputMode::Plain`] (a non-TTY
    /// or `--plain` stream - piped, redirected, or a CI log) and
    /// [`OutputMode::Json`] (a machine/batch invocation) both have no
    /// interactive console to show that state in, so both keep `bounded`: an
    /// append-only or machine caller must get a deterministic failure instead
    /// of hanging forever on a missing clock. Any future headless
    /// bounded-wait policy should be an explicit, separate opt-in - never
    /// implicitly shared with the interactive session the way the old fixed
    /// 60s constant was.
    #[must_use]
    pub const fn wait_budget(self, bounded: Duration) -> Duration {
        if self.mode.allows_progress_drawing() {
            NO_TIMEOUT
        } else {
            bounded
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
        Self::new(mode, Theme::detect_stderr(mode), quiet)
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

    /// Product decision 6: only `Rich` - the true interactive TTY console -
    /// gets the unbounded wait. `Plain` (piped/non-TTY/CI) and `Json`
    /// (machine callers) have no interactive console to show a "waiting"
    /// state in, so both must keep a bounded, deterministic failure instead
    /// of hanging forever on a missing clock.
    #[test]
    fn wait_budget_is_bounded_unless_rich() {
        let bounded = Duration::from_secs(60);
        let rich = OutputContext::new(OutputMode::Rich, Theme::new(ColorCapability::None), false);
        let plain = OutputContext::new(OutputMode::Plain, Theme::new(ColorCapability::None), false);
        let json = OutputContext::new(OutputMode::Json, Theme::new(ColorCapability::None), false);

        assert!(rich.wait_budget(bounded) > bounded);
        assert_eq!(plain.wait_budget(bounded), bounded);
        assert_eq!(json.wait_budget(bounded), bounded);
    }
}
