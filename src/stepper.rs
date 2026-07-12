//! The in-place startup stepper (Phase 0's deferred piece): renders a long
//! verb's (`run`, `simulation run`, `deploy`) bring-up phases as one line
//! per phase under the identity line, using the shared
//! [`crate::progress`]/[`crate::theme`] primitives - the same visual system
//! the board render and a later full-screen TUI share.
//!
//! Each phase is one line: the active phase animates (a spinner, plus an
//! optional `done/total` item counter set via [`Stepper::progress`]);
//! a completed phase becomes a `✓` line; a skipped phase (nothing to do)
//! becomes a dimmed `·` line; a failed phase becomes a `✗` line with the
//! error printed beneath it, and short-circuits the whole sequence.
//!
//! Respects the [`OutputMode`] matrix exactly like [`crate::progress`]:
//! under [`OutputMode::Rich`] every phase redraws in place (an
//! [`indicatif::MultiProgress`] group, one bar per phase); under
//! [`OutputMode::Plain`]/a non-TTY the same information prints as
//! append-only lines with no redraw; under [`OutputMode::Json`] the stepper
//! is entirely silent, matching the JSON contract's promise that stderr
//! carries nothing extra.
//!
//! `run`/`simulation run` clear the sequence on `Initialized` and hand off
//! to the existing board render (a later slice replaces that hand-off with
//! the ratatui TUI); `deploy` has no board to hand off to, so it just prints
//! its own result summary after the last phase completes.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::output_mode::OutputMode;
use crate::progress;
use crate::theme::{Role, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseState {
    Pending,
    Active,
    Done,
    Skipped,
    Failed,
}

struct RichPhase {
    bar: ProgressBar,
    state: PhaseState,
}

enum Backend {
    /// One live-redrawing line per phase, all in place under a single
    /// [`MultiProgress`] group.
    Rich {
        _multi: MultiProgress,
        phases: Vec<RichPhase>,
    },
    /// Append-only: each transition prints exactly one plain line, never
    /// redrawn - matches [`crate::progress::Handle::Plain`]'s shape.
    Plain,
    /// No output at all - matches [`crate::progress::Handle::Silent`].
    Json,
}

/// One stage's contribution to [`Stepper::drive_participant_phases`]:
/// whether it spawned any members at all (`false` -> the phase reads
/// "skipped, nothing to start"), and which board ids to poll for `Ready`
/// before considering it complete (empty -> spawned but nothing to
/// explicitly wait for, e.g. the router - completes instantly instead of
/// reading as skipped).
#[derive(Debug, Clone, Default)]
pub struct StagePhase {
    pub has_members: bool,
    pub ready_ids: Vec<String>,
}

impl StagePhase {
    #[must_use]
    pub fn new(has_members: bool, ready_ids: Vec<String>) -> Self {
        Self {
            has_members,
            ready_ids,
        }
    }
}

/// The startup stepper. Construct with the phase labels in order
/// ([`Stepper::new`]), then drive it phase by phase as the verb's bring-up
/// actually reaches each one - the stepper never advances on its own.
pub struct Stepper {
    theme: Theme,
    labels: Vec<String>,
    backend: Backend,
    /// Set by [`Self::fail`], sticky for the rest of this `Stepper`'s life.
    /// Lets a caller driving several phases in sequence (e.g.
    /// [`Self::drive_participant_phases`], or `run`/`simulation run`'s own
    /// final `Initialized` phase) check [`Self::has_failed`] before
    /// advancing further, without needing per-backend state (`Backend::Plain`/
    /// `Backend::Json` track no phase state of their own).
    failed: bool,
}

/// Build a one-line phase style: an animated `{spinner}` for the active
/// phase, or a static `{prefix}` glyph (✓/·/✗) for a settled one, both
/// followed by `{msg}`, tagged with `glyph_role`'s color when the terminal
/// supports it (falls back to an untagged template under
/// [`crate::theme::ColorCapability::None`], exactly like
/// [`crate::progress`]'s own spinner style).
fn phase_style(theme: Theme, glyph_role: Role, animated: bool) -> ProgressStyle {
    let glyph_field = if animated { "spinner" } else { "prefix" };
    let template = match theme.indicatif_tag(glyph_role) {
        Some(tag) => format!("{{{glyph_field}:.{tag}}} {{msg}}"),
        None => format!("{{{glyph_field}}} {{msg}}"),
    };
    let style = ProgressStyle::with_template(&template).unwrap_or_else(|_| {
        if animated {
            ProgressStyle::default_spinner()
        } else {
            ProgressStyle::with_template("{prefix} {msg}").expect("static template is valid")
        }
    });
    if animated {
        style.tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓")
    } else {
        style
    }
}

impl Stepper {
    #[must_use]
    pub fn new(labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let labels: Vec<String> = labels.into_iter().map(Into::into).collect();
        let theme = Theme::detect_stderr();
        let backend = match progress::current_mode() {
            OutputMode::Json => Backend::Json,
            OutputMode::Plain => Backend::Plain,
            OutputMode::Rich => {
                let multi = MultiProgress::new();
                let phases = labels
                    .iter()
                    .map(|label| {
                        let bar = multi.add(ProgressBar::new_spinner());
                        bar.set_style(phase_style(theme, Role::Muted, false));
                        bar.set_prefix("·");
                        bar.set_message(theme.muted(label));
                        RichPhase {
                            bar,
                            state: PhaseState::Pending,
                        }
                    })
                    .collect();
                Backend::Rich {
                    _multi: multi,
                    phases,
                }
            }
        };
        Self {
            theme,
            labels,
            backend,
            failed: false,
        }
    }

    /// Whether [`Self::fail`] has been called yet - sticky for the rest of
    /// this `Stepper`'s life. Callers driving several phases in sequence
    /// check this before advancing to a further phase (e.g. `Initialized`)
    /// so a failure earlier in the sequence is not silently papered over by
    /// an unconditional final "success" phase.
    #[must_use]
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    fn label(&self, index: usize) -> &str {
        self.labels[index].as_str()
    }

    /// Mark `index` the active phase: animated spinner (Rich) or a single
    /// plain "starting ..." line (Plain).
    pub fn start(&mut self, index: usize) {
        let label = self.label(index).to_string();
        match &mut self.backend {
            Backend::Rich { phases, .. } => {
                let phase = &mut phases[index];
                phase.state = PhaseState::Active;
                phase
                    .bar
                    .set_style(phase_style(self.theme, Role::Steel, true));
                phase
                    .bar
                    .enable_steady_tick(std::time::Duration::from_millis(90));
                phase.bar.set_message(self.theme.text_primary(&label));
            }
            Backend::Plain => eprintln!("{label}..."),
            Backend::Json => {}
        }
    }

    /// Update the active phase's message with a live `done/total` item
    /// counter (e.g. "Starting services (2/4)"). No-op under Plain/Json -
    /// those only ever print one line per transition, never a live count.
    pub fn progress(&mut self, index: usize, done: usize, total: usize) {
        if let Backend::Rich { phases, .. } = &mut self.backend {
            let label = self.labels[index].as_str();
            let phase = &mut phases[index];
            if phase.state == PhaseState::Active {
                phase.bar.set_message(
                    self.theme
                        .text_primary(&format!("{label} ({done}/{total})")),
                );
            }
        }
    }

    /// Mark `index` complete: a `✓` line.
    pub fn complete(&mut self, index: usize) {
        let label = self.label(index).to_string();
        match &mut self.backend {
            Backend::Rich { phases, .. } => {
                let phase = &mut phases[index];
                phase.state = PhaseState::Done;
                phase.bar.disable_steady_tick();
                phase
                    .bar
                    .set_style(phase_style(self.theme, Role::Success, false));
                phase.bar.set_prefix(self.theme.success("\u{2713}"));
                phase.bar.set_message(self.theme.text(&label));
            }
            Backend::Plain => eprintln!("{} {label}", self.theme.success("\u{2713}")),
            Backend::Json => {}
        }
    }

    /// Mark `index` skipped (nothing to do this run): a dimmed `·` line
    /// carrying `note` instead of the phase's own label (e.g. "Artifacts up
    /// to date" / "Nothing to build").
    pub fn skip(&mut self, index: usize, note: impl Into<String>) {
        let note = note.into();
        match &mut self.backend {
            Backend::Rich { phases, .. } => {
                let phase = &mut phases[index];
                phase.state = PhaseState::Skipped;
                phase.bar.disable_steady_tick();
                phase
                    .bar
                    .set_style(phase_style(self.theme, Role::Muted, false));
                phase.bar.set_prefix("\u{b7}");
                phase.bar.set_message(self.theme.muted(&note));
            }
            Backend::Plain => eprintln!("\u{b7} {note}"),
            Backend::Json => {}
        }
    }

    /// Mark `index` failed: a `✗` line with `error` printed beneath it, and
    /// short-circuit - the caller stops driving the stepper after this.
    pub fn fail(&mut self, index: usize, error: impl AsRef<str>) {
        self.failed = true;
        let label = self.label(index).to_string();
        let error = error.as_ref();
        match &mut self.backend {
            Backend::Rich { phases, .. } => {
                let phase = &mut phases[index];
                phase.state = PhaseState::Failed;
                phase.bar.disable_steady_tick();
                phase
                    .bar
                    .set_style(phase_style(self.theme, Role::Error, false));
                phase.bar.set_prefix(self.theme.error("\u{2717}"));
                phase.bar.set_message(self.theme.text(&label));
                phase.bar.println(self.theme.error(&format!("  {error}")));
            }
            Backend::Plain => {
                eprintln!("{} {label}", self.theme.error("\u{2717}"));
                eprintln!("  {error}");
            }
            Backend::Json => {}
        }
    }

    /// Drive a contiguous run of "waiting for participants" phases live from
    /// the board (Part 4): for each `stages[offset]`, the phase at
    /// `base_index + offset` becomes active, polls the board until every id
    /// in its `ready_ids` is `Ready` (or one goes `Failed`, which fails the
    /// phase and stops driving further ones), then completes before moving
    /// on. `ready_ids` can be empty for a stage that DID spawn members but
    /// has nothing to explicitly wait for (e.g. the router: spawn-is-ready,
    /// not heartbeat-gated - see `supervisor::SupervisionStage`'s docs) -
    /// that still completes instantly rather than reading as skipped;
    /// [`StagePhase::has_members`] `false` is the genuine "nothing to
    /// start" case. Consumes and returns `self` so the caller can keep
    /// driving mode-specific phases afterward (`Initialized`, or
    /// `simulation run`'s clock wait) - shared by `run` and `simulation run`
    /// so both use one polling implementation instead of each hand-rolling
    /// it against the board.
    pub async fn drive_participant_phases(
        mut self,
        board: &crate::supervisor::BoardBackend,
        base_index: usize,
        stages: Vec<StagePhase>,
    ) -> Self {
        for (offset, stage) in stages.into_iter().enumerate() {
            let phase_index = base_index + offset;
            let ids = stage.ready_ids;
            self.start(phase_index);
            if !stage.has_members {
                self.skip(phase_index, "nothing to start");
                continue;
            }
            if ids.is_empty() {
                // Spawned, but nothing to explicitly wait for (spawn-is-ready).
                self.complete(phase_index);
                continue;
            }
            loop {
                let snapshot = board.snapshot();
                let ready = ids
                    .iter()
                    .filter(|id| {
                        snapshot
                            .participants
                            .get(id.as_str())
                            .is_some_and(|status| {
                                status.state == crate::supervisor::ParticipantState::Ready
                            })
                    })
                    .count();
                self.progress(phase_index, ready, ids.len());
                if ready == ids.len() {
                    break;
                }
                let failed = ids.iter().any(|id| {
                    snapshot
                        .participants
                        .get(id.as_str())
                        .is_some_and(|status| {
                            status.state == crate::supervisor::ParticipantState::Failed
                        })
                });
                if failed {
                    self.fail(phase_index, "one or more participants failed to start");
                    return self;
                }
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            self.complete(phase_index);
        }
        self
    }

    /// Clear the whole sequence (Rich only - erases every phase line) so
    /// the caller can hand off to the board render / TUI without the
    /// stepper's lines lingering above it. Plain's lines already scrolled
    /// by (append-only); Json never printed anything.
    pub fn clear(self) {
        if let Backend::Rich { phases, .. } = self.backend {
            for phase in phases {
                phase.bar.finish_and_clear();
            }
        }
    }
}
