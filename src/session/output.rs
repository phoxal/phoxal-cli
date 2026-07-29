//! The explicit output contract for one invocation: whether stderr is an
//! interactive terminal and the theme selected for it.
//!
//! Built once in [`crate::commands::dispatch`] and threaded explicitly into
//! [`super::controller::SessionController`], `AppContext::ui`, and helpers
//! that may draw progress. Interactive foreground sessions are admitted only
//! on a real TTY, so the controller itself has exactly one renderer: the TUI.

use std::time::Duration;

pub use phoxal_cli_supervisor::WaitBudget;
use phoxal_cli_ui::Theme;

/// The immutable output contract for one `run`/`simulation webots run` invocation.
#[derive(Debug, Clone, Copy)]
pub struct OutputContext {
    pub interactive: bool,
    pub theme: Theme,
}

impl OutputContext {
    #[must_use]
    pub const fn new(interactive: bool, theme: Theme) -> Self {
        Self { interactive, theme }
    }

    /// Whether finite-command presentation may use terminal decoration.
    #[must_use]
    pub const fn decorated(self) -> bool {
        self.interactive
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

    /// Build from stderr's terminal state. Called once in
    /// [`crate::commands::dispatch`]; other callers should prefer [`Self::new`]
    /// so tests stay deterministic.
    #[must_use]
    pub fn compute(is_tty: bool) -> Self {
        Self::new(is_tty, Theme::detect_stderr(is_tty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_ui::ColorCapability;
    use std::time::Instant;

    #[test]
    fn compute_tracks_the_terminal() {
        let ctx = OutputContext::compute(true);
        assert!(ctx.interactive);

        assert!(!OutputContext::compute(false).interactive);
    }

    #[test]
    fn new_is_an_immutable_constructor() {
        let ctx = OutputContext::new(false, Theme::new(ColorCapability::None));
        assert!(!ctx.interactive);
    }

    /// Product decision 6: only the interactive TTY console gets the
    /// unbounded wait. Batch callers keep a bounded, deterministic failure.
    #[test]
    fn wait_budget_is_bounded_unless_rich() {
        let bounded = Duration::from_secs(60);
        let interactive = OutputContext::new(true, Theme::new(ColorCapability::None));
        let batch = OutputContext::new(false, Theme::new(ColorCapability::None));

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
