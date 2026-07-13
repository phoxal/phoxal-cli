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

use std::time::{Duration, Instant};

use crate::commands::MessageFormat;
use crate::output_mode::OutputMode;
use crate::theme::Theme;

/// A readiness/stage-wait budget for an interactive session (Product decision
/// 6: "no unconditional 60-second teardown in an interactive session").
///
/// Replaces a one-year `Duration` sentinel for "no timeout" (finding D2): a
/// magic duration still technically times out (`Instant + Duration` would
/// eventually panic on overflow, and every caller had to remember never to
/// print it as a real deadline) and obscures the intended semantics. This
/// type makes "no deadline at all" a distinct, explicit state a caller must
/// handle, rather than a very large number it might format or compare
/// incorrectly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitBudget {
    /// No deadline at all: a missing clock/participant is a named
    /// `waiting`/`degraded` console state for as long as the operator leaves
    /// the session open, not a kill.
    Unbounded,
    Bounded(Duration),
}

impl Default for WaitBudget {
    /// `Bounded(Duration::default())` (an already-elapsed deadline) rather
    /// than `Unbounded` - a derived `Default` (e.g. `SupervisionStage`'s) must
    /// never silently produce a wait with no deadline at all; every real
    /// caller sets this explicitly via `OutputContext::wait_budget`.
    fn default() -> Self {
        Self::Bounded(Duration::default())
    }
}

impl WaitBudget {
    /// The deadline this budget implies starting from `now`, or `None` if
    /// [`Self::Unbounded`] - there is no `Instant` a caller should ever
    /// compare against.
    #[must_use]
    pub fn deadline_from(self, now: Instant) -> Option<Instant> {
        match self {
            Self::Unbounded => None,
            Self::Bounded(duration) => Some(now + duration),
        }
    }
}

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
    /// [`WaitBudget::Unbounded`]: a missing clock/participant becomes a named
    /// `waiting`/`degraded` console state, never an automatic teardown,
    /// because an operator is watching it. [`OutputMode::Plain`] (a non-TTY
    /// or `--plain` stream - piped, redirected, or a CI log) and
    /// [`OutputMode::Json`] (a machine/batch invocation) both have no
    /// interactive console to show that state in, so both keep `Bounded`: an
    /// append-only or machine caller must get a deterministic failure instead
    /// of hanging forever on a missing clock. Any future headless
    /// bounded-wait policy should be an explicit, separate opt-in - never
    /// implicitly shared with the interactive session the way the old fixed
    /// 60s constant was.
    #[must_use]
    pub const fn wait_budget(self, bounded: Duration) -> WaitBudget {
        if self.mode.allows_progress_drawing() {
            WaitBudget::Unbounded
        } else {
            WaitBudget::Bounded(bounded)
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

        assert_eq!(rich.wait_budget(bounded), WaitBudget::Unbounded);
        assert_eq!(plain.wait_budget(bounded), WaitBudget::Bounded(bounded));
        assert_eq!(json.wait_budget(bounded), WaitBudget::Bounded(bounded));
    }

    /// D2: `Unbounded` has no deadline at all - a caller must handle that
    /// explicitly rather than comparing against a very large `Instant`.
    #[test]
    fn unbounded_has_no_deadline_bounded_has_one() {
        let now = Instant::now();
        assert_eq!(WaitBudget::Unbounded.deadline_from(now), None);
        let bounded = WaitBudget::Bounded(Duration::from_secs(5));
        assert_eq!(
            bounded.deadline_from(now),
            Some(now + Duration::from_secs(5))
        );
    }
}
