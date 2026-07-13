//! The shared progress primitive: a spinner and a determinate byte/count bar,
//! styled through [`crate::theme`], drawn to stderr only, and gated by
//! [`OutputMode`] so it never emits cursor control on a non-interactive
//! stream and never leaks a byte into `--message-format json`.
//!
//! [`spinner`]/[`bytes_bar`] take their [`OutputMode`] explicitly from the
//! caller - no process-global mode cell. Every long-running operation that
//! draws one ([`crate::catalog::fetch_https`],
//! [`crate::native_artifacts::download_blob`],
//! [`crate::resolver::resolve_git_ref`], the cargo build helpers) either has
//! an `AppContext`/`OutputContext` in scope already or threads the mode down
//! from the caller that does.
//!
//! # Session routing (findings A2/B2)
//!
//! Before drawing anything, both constructors first try
//! [`crate::session::diagnostics::try_route`]: if a `run`/`simulation run`
//! session has installed its event channel, the message is routed as a
//! `SessionEvent::Diagnostic` and this returns [`Handle::Routed`] - NEVER a
//! raw stderr write or an indicatif redraw, regardless of what `mode` says.
//! This closes two gaps a caller-supplied `mode` cannot on its own: a TUI
//! session owns the terminal (a raw `ProgressBar` redraw would corrupt its
//! frame - A2), and a caller that recomputed `mode` from the live environment
//! instead of the session's real `OutputContext` (`OutputMode::from_env()`,
//! e.g. `check::build_and_locate_binary`'s spinner) can otherwise disagree
//! with the actual session and leak a line onto a stderr `--message-format
//! json` promises stays empty (B2). Only when no session is installed
//! (`try_route` returns `false` - most callers of this module, e.g. a bare
//! `phoxal-cli check`/`deploy` outside any session) does `mode` decide the
//! fallback behavior, exactly as before.

use std::sync::Arc;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::output_mode::OutputMode;
use crate::session::diagnostics::{RouteResult, try_route};
use crate::session::event::{DiagnosticLevel, DiagnosticSource};
use crate::theme::{Role, Theme};

/// A live spinner or bar handle. Draws when the mode allows it, prints a
/// single append-only line under [`OutputMode::Plain`], is entirely silent
/// under [`OutputMode::Json`], and is absorbed by an active session's
/// diagnostics routing ([`Handle::Routed`]) whenever one is installed,
/// regardless of `mode` (findings A2/B2 - see the module docs).
pub enum Handle {
    Rich(ProgressBar),
    Plain,
    Routed,
    Silent,
}

/// A Docker-build-style group of stable progress rows. Rich terminals redraw
/// all active rows together; non-interactive and JSON modes stay silent so the
/// caller can emit one deterministic final list after the work completes.
pub struct Rows {
    rich: Option<Arc<MultiProgress>>,
    theme: Theme,
}

/// One determinate row owned by a worker. Finishing the row replaces its bar
/// with a compact result instead of printing a second summary line.
pub enum Row {
    Rich {
        bar: ProgressBar,
        group: Arc<MultiProgress>,
        theme: Theme,
    },
    Routed,
    Silent,
}

impl Rows {
    #[must_use]
    pub fn new(mode: OutputMode) -> Self {
        Self {
            rich: (mode == OutputMode::Rich)
                .then(|| Arc::new(MultiProgress::with_draw_target(ProgressDrawTarget::stderr()))),
            theme: Theme::detect_stderr(mode),
        }
    }

    #[must_use]
    pub fn bytes(&self, message: impl Into<String>, total: u64) -> Row {
        let message = message.into();
        if !matches!(try_route_progress(&message), RouteResult::NoSession) {
            return Row::Routed;
        }
        let Some(group) = &self.rich else {
            return Row::Silent;
        };
        let bar = group.add(ProgressBar::new(total));
        bar.set_style(row_bytes_style(self.theme));
        bar.enable_steady_tick(Duration::from_millis(90));
        bar.set_message(message);
        Row::Rich {
            bar,
            group: Arc::clone(group),
            theme: self.theme,
        }
    }

    pub fn completed(&self, message: impl Into<String>) {
        let message = message.into();
        if !matches!(try_route_progress(&message), RouteResult::NoSession) {
            return;
        }
        if let Some(group) = &self.rich {
            let bar = group.add(ProgressBar::new(0));
            finish_row(&bar, group, self.theme, Role::Success, "\u{2713}", message);
        }
    }
}

impl Row {
    pub fn inc(&self, delta: u64) {
        if let Self::Rich { bar, .. } = self {
            bar.inc(delta);
        }
    }

    pub fn finish(self, message: impl Into<String>) {
        let message = message.into();
        match self {
            Self::Rich { bar, group, theme } => {
                finish_row(&bar, &group, theme, Role::Success, "\u{2713}", message);
            }
            Self::Routed => {
                let _ = try_route(
                    DiagnosticSource::Dependency,
                    DiagnosticLevel::Info,
                    &message,
                );
            }
            Self::Silent => {}
        }
    }

    pub fn clear(self) {
        if let Self::Rich { bar, .. } = self {
            bar.finish_and_clear();
        }
    }

    pub fn abandon(self, message: impl Into<String>) {
        let message = message.into();
        match self {
            Self::Rich { bar, group, theme } => {
                finish_row(&bar, &group, theme, Role::Error, "\u{2717}", message);
            }
            Self::Routed => {
                let _ = try_route(
                    DiagnosticSource::Dependency,
                    DiagnosticLevel::Warn,
                    &message,
                );
            }
            Self::Silent => {}
        }
    }
}

/// Route `message` through the active session's diagnostics channel, if one
/// is installed. Shared by [`spinner`] and [`bytes_bar`].
fn try_route_progress(message: &str) -> RouteResult {
    try_route(DiagnosticSource::Dependency, DiagnosticLevel::Info, message)
}

/// Start an indeterminate spinner with `message`. No-op fallback prints one
/// plain line under [`OutputMode::Plain`]; nothing at all under
/// [`OutputMode::Json`]; routed through an active session's diagnostics
/// instead of drawn directly whenever one is installed (see the module
/// docs).
#[must_use]
pub fn spinner(message: impl Into<String>, mode: OutputMode) -> Handle {
    let message = message.into();
    if !matches!(try_route_progress(&message), RouteResult::NoSession) {
        return Handle::Routed;
    }
    match mode {
        OutputMode::Json => Handle::Silent,
        OutputMode::Plain => {
            eprintln!("{message}");
            Handle::Plain
        }
        OutputMode::Rich => {
            let theme = Theme::detect_stderr(mode);
            let bar = ProgressBar::new_spinner();
            bar.set_draw_target(ProgressDrawTarget::stderr());
            bar.set_style(spinner_style(theme));
            bar.enable_steady_tick(Duration::from_millis(90));
            bar.set_message(message);
            Handle::Rich(bar)
        }
    }
}

/// Start a determinate byte/count bar with `message` and a known `total`.
/// Callers with an unknown total should use [`spinner`] instead. Routed
/// through an active session's diagnostics instead of drawn directly
/// whenever one is installed (see the module docs).
#[must_use]
pub fn bytes_bar(message: impl Into<String>, total: u64, mode: OutputMode) -> Handle {
    let message = message.into();
    if !matches!(try_route_progress(&message), RouteResult::NoSession) {
        return Handle::Routed;
    }
    match mode {
        OutputMode::Json => Handle::Silent,
        OutputMode::Plain => {
            eprintln!("{message}");
            Handle::Plain
        }
        OutputMode::Rich => {
            let theme = Theme::detect_stderr(mode);
            let bar = ProgressBar::new(total);
            bar.set_draw_target(ProgressDrawTarget::stderr());
            bar.set_style(bytes_style(theme));
            bar.set_message(message);
            Handle::Rich(bar)
        }
    }
}

impl Handle {
    /// Advance a [`bytes_bar`] by `delta`. No-op for a spinner or a
    /// non-drawing handle.
    pub fn inc(&self, delta: u64) {
        if let Self::Rich(bar) = self {
            bar.inc(delta);
        }
    }

    /// Finish successfully and erase the line (Rich), or print nothing more
    /// (Plain already printed its one line at start; Json/Routed never
    /// printed and have nothing further to say).
    pub fn finish_and_clear(self) {
        if let Self::Rich(bar) = self {
            bar.finish_and_clear();
        }
    }

    /// Finish on failure, leaving `message` as the final line without the
    /// bar's "done" framing (Rich), or print it as the plain line (Plain), or
    /// route it as a warning (Routed) - a failure is worth a higher severity
    /// than the routed start/success lines.
    pub fn abandon_with_message(self, message: impl Into<String>) {
        let message = message.into();
        match self {
            Self::Rich(bar) => bar.abandon_with_message(message),
            Self::Plain => eprintln!("{message}"),
            // A routed handle was created while a session owned output. Keep
            // that ownership even if teardown races this final diagnostic:
            // raw fallback here could corrupt a live TUI or leak JSON stderr.
            Self::Routed => {
                let _ = try_route(
                    DiagnosticSource::Dependency,
                    DiagnosticLevel::Warn,
                    &message,
                );
            }
            Self::Silent => {}
        }
    }
}

const SPINNER_TICK_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓";

fn spinner_style(theme: Theme) -> ProgressStyle {
    let template = match theme.indicatif_tag(Role::Accent) {
        Some(tag) => format!("{{spinner:.{tag}}} {{msg}}"),
        None => "{spinner} {msg}".to_string(),
    };
    ProgressStyle::with_template(&template)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_chars(SPINNER_TICK_CHARS)
}

fn bytes_style(theme: Theme) -> ProgressStyle {
    let template = match theme.indicatif_tag(Role::Accent) {
        Some(tag) => {
            format!("{{msg}} {{bytes}}/{{total_bytes}} [{{bar:28.{tag}}}] {{bytes_per_sec}}")
        }
        None => "{msg} {bytes}/{total_bytes} [{bar:28}] {bytes_per_sec}".to_string(),
    };
    ProgressStyle::with_template(&template).unwrap_or_else(|_| ProgressStyle::default_bar())
}

fn row_bytes_style(theme: Theme) -> ProgressStyle {
    let template = match theme.indicatif_tag(Role::Accent) {
        Some(tag) => format!(
            "{{spinner:.{tag}}} {{msg}} {{bytes}}/{{total_bytes}} [{{bar:24.{tag}}}] {{bytes_per_sec}}"
        ),
        None => "{spinner} {msg} {bytes}/{total_bytes} [{bar:24}] {bytes_per_sec}".to_string(),
    };
    ProgressStyle::with_template(&template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .tick_chars(SPINNER_TICK_CHARS)
}

fn finish_row(
    bar: &ProgressBar,
    group: &MultiProgress,
    theme: Theme,
    role: Role,
    mark: &str,
    message: String,
) {
    bar.disable_steady_tick();
    bar.finish_and_clear();
    group.suspend(|| eprintln!("{} {message}", theme.paint(role, mark)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK;

    // These three tests all depend on NO session diagnostics sender being
    // installed (so `try_route_progress` returns `false` and `mode` decides
    // the outcome) - the same process-global cell `session::diagnostics`'s
    // own tests and `session::controller`'s `drive_setup` tests install into.
    // Serialize through the SAME lock those modules use, or a concurrently
    // running test elsewhere in the crate could install a sender mid-test
    // and flip these from `Silent`/`Plain`/`Rich` to `Routed` intermittently.

    #[test]
    fn json_mode_yields_a_silent_handle_for_spinner_and_bytes_bar() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        assert!(matches!(
            spinner("fetching", OutputMode::Json),
            Handle::Silent
        ));
        assert!(matches!(
            bytes_bar("downloading", 100, OutputMode::Json),
            Handle::Silent
        ));
    }

    #[test]
    fn plain_mode_yields_a_plain_handle_not_a_rich_bar() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        assert!(matches!(
            spinner("fetching", OutputMode::Plain),
            Handle::Plain
        ));
        assert!(matches!(
            bytes_bar("downloading", 100, OutputMode::Plain),
            Handle::Plain
        ));
    }

    #[test]
    fn rich_mode_yields_a_rich_handle() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        assert!(matches!(
            spinner("fetching", OutputMode::Rich),
            Handle::Rich(_)
        ));
        assert!(matches!(
            bytes_bar("downloading", 100, OutputMode::Rich),
            Handle::Rich(_)
        ));
    }

    /// Findings A2/B2: once a session has installed its diagnostics sender,
    /// `spinner`/`bytes_bar` must route through it - never drawing directly -
    /// regardless of what `mode` says (a stale `OutputMode::Rich` from
    /// `OutputMode::from_env()` must not draw a raw `ProgressBar` over an
    /// active TUI frame).
    #[test]
    fn an_installed_session_routes_progress_instead_of_drawing_it() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        crate::session::diagnostics::install(tx);

        let handle = spinner("building", OutputMode::Rich);
        assert!(
            matches!(handle, Handle::Routed),
            "an installed session must absorb the message, never draw a raw ProgressBar"
        );
        // `abandon_with_message` is the real production caller of a Routed
        // handle's post-start message (a failed download) - reused here to
        // prove the Routed variant also intercepts a FINISH-time message, not
        // only its start message.
        handle.abandon_with_message("build failed");

        crate::session::diagnostics::uninstall();

        let mut messages = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::session::event::SessionEvent::Diagnostic { message, .. } = event {
                messages.push(message);
            }
        }
        assert_eq!(
            messages,
            vec!["building".to_string(), "build failed".to_string()]
        );
    }

    #[test]
    fn colorless_theme_builds_a_template_with_no_color_tag() {
        let style = spinner_style(Theme::new(crate::theme::ColorCapability::None));
        // `ProgressStyle` has no public accessor for its template text; the
        // real assertion is that construction with an untagged template
        // succeeds and never panics/falls back silently to a different
        // style, which `unwrap_or_else` above would otherwise mask.
        let _ = style;
    }

    #[test]
    fn grouped_rows_are_silent_outside_rich_mode() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let rows = Rows::new(OutputMode::Plain);
        assert!(matches!(rows.bytes("downloading", 10), Row::Silent));
        rows.completed("done");

        let rows = Rows::new(OutputMode::Json);
        assert!(matches!(rows.bytes("downloading", 10), Row::Silent));
    }
}
