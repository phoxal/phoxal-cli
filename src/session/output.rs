//! The explicit output contract for one invocation: whether terminal
//! decoration is interactive, the theme it uses, and whether `--quiet` was
//! requested.
//!
//! Built once in [`crate::commands::dispatch`] and threaded explicitly into
//! [`super::controller::SessionController`], `AppContext::ui`, and helpers
//! that may draw progress. Interactive foreground sessions are admitted only
//! on a real TTY, so the controller itself has exactly one renderer: the TUI.

use std::time::{Duration, Instant};

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
    pub interactive: bool,
    pub theme: Theme,
    pub quiet: bool,
}

impl OutputContext {
    #[must_use]
    pub const fn new(interactive: bool, theme: Theme, quiet: bool) -> Self {
        Self {
            interactive,
            theme,
            quiet,
        }
    }

    /// Whether finite-command presentation may use color and redraw progress.
    /// A quiet live session still owns its TUI, while quiet finite commands
    /// preserve their append-only, uncolored output.
    #[must_use]
    pub const fn decorated(self) -> bool {
        self.interactive && !self.quiet
    }

    /// The wait budget for an interactive-session readiness/stage wait
    /// (Product decision 6). Only a true interactive TTY console that renders
    /// a live "waiting" state gets
    /// [`WaitBudget::Unbounded`]: a missing clock/participant becomes a named
    /// `waiting`/`degraded` console state, never an automatic teardown,
    /// because an operator is watching it. Non-interactive callers have no
    /// console to show that state in, so they keep `Bounded`: a batch caller
    /// must get a deterministic failure instead of hanging forever. Any future headless
    /// bounded-wait policy should be an explicit, separate opt-in - never
    /// implicitly shared with the interactive session the way the old fixed
    /// 60s constant was.
    #[must_use]
    pub const fn wait_budget(self, bounded: Duration) -> WaitBudget {
        if self.interactive {
            WaitBudget::Unbounded
        } else {
            WaitBudget::Bounded(bounded)
        }
    }

    /// Build from the terminal and global presentation flags. Called once in
    /// [`crate::commands::dispatch`]; other callers should prefer [`Self::new`]
    /// so tests stay deterministic.
    #[must_use]
    pub fn compute(is_tty: bool, plain: bool, quiet: bool) -> Self {
        let interactive = is_tty && !plain;
        Self::new(interactive, Theme::detect_stderr(interactive), quiet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorCapability;

    #[test]
    fn compute_respects_terminal_and_global_flags() {
        let ctx = OutputContext::compute(true, false, false);
        assert!(ctx.interactive);
        assert!(!ctx.quiet);

        assert!(!OutputContext::compute(false, false, false).interactive);
        assert!(!OutputContext::compute(true, true, false).interactive);
        assert!(OutputContext::compute(true, false, true).interactive);
        assert!(OutputContext::compute(true, false, true).quiet);
        assert!(!OutputContext::compute(true, false, true).decorated());
    }

    #[test]
    fn new_is_a_plain_immutable_constructor() {
        let ctx = OutputContext::new(false, Theme::new(ColorCapability::None), true);
        assert!(!ctx.interactive);
        assert!(ctx.quiet);
    }

    /// Product decision 6: only the interactive TTY console gets the
    /// unbounded wait. Batch callers keep a bounded, deterministic failure.
    #[test]
    fn wait_budget_is_bounded_unless_rich() {
        let bounded = Duration::from_secs(60);
        let interactive = OutputContext::new(true, Theme::new(ColorCapability::None), false);
        let batch = OutputContext::new(false, Theme::new(ColorCapability::None), false);

        assert_eq!(interactive.wait_budget(bounded), WaitBudget::Unbounded);
        assert_eq!(batch.wait_budget(bounded), WaitBudget::Bounded(bounded));
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
