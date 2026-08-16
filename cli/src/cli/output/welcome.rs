//! The branded startup checklist that runs before the dashboard.
//!
//! One line per startup step, printed once it is finished; exactly one line -
//! the active one - is rewritten in place while it runs. There is no alternate
//! screen, no raw mode, and no full-frame repaint here: the operator's
//! scrollback keeps every completed step, and the terminal is handed over
//! untouched when the dashboard takes it (phoxal-cli#252).
//!
//! The presenter is deliberately dumb: it owns rendering and nothing else.
//! Which step is running, and why, is decided by
//! [`crate::application::startup`].

#![deny(clippy::print_stdout)]

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use console::Term;
use phoxal_cli_ui::Theme;

use super::brand;
use super::plain::Ui;

const SPINNER_UNICODE: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_ASCII: &[char] = &['|', '/', '-', '\\'];

/// The column the step detail starts at. Wide enough for the longest label.
const LABEL_WIDTH: usize = 17;
/// The column budget the live detail is truncated into.
const DETAIL_WIDTH: usize = 36;

/// Which kind of execution is starting. It selects the step list - only a
/// simulation has a simulator to bring up - and labels the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Native,
    Webots,
}

impl Mode {
    const fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Webots => "webots",
        }
    }

    const fn steps(self) -> &'static [StepId] {
        match self {
            Self::Native => &[
                StepId::Project,
                StepId::PrepareRuntime,
                StepId::Supervisor,
                StepId::Runtimes,
            ],
            Self::Webots => &[
                StepId::Project,
                StepId::PrepareRuntime,
                StepId::Supervisor,
                StepId::Webots,
                StepId::Runtimes,
            ],
        }
    }
}

/// The fixed startup sequence an operator watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepId {
    Project,
    PrepareRuntime,
    Supervisor,
    Webots,
    /// The runtimes this client launched, counted against the presence the
    /// supervisor reports for them.
    Runtimes,
}

impl StepId {
    const fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::PrepareRuntime => "Prepare runtime",
            Self::Supervisor => "Supervisor",
            Self::Webots => "Webots",
            Self::Runtimes => "Runtimes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    Pending,
    Active,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
struct Step {
    id: StepId,
    state: StepState,
    detail: String,
    started: Option<Instant>,
    elapsed: Option<Duration>,
}

impl Step {
    fn elapsed(&self) -> Option<Duration> {
        self.elapsed
            .or_else(|| self.started.map(|started| started.elapsed()))
    }
}

/// The startup checklist plus the view that renders it.
pub(crate) struct Welcome {
    view: Box<dyn View>,
    steps: Vec<Step>,
    log: Option<File>,
    spinner: usize,
    unicode: bool,
    /// Set once the terminal has been released. Every render is a no-op after
    /// that, so a tick that raced the handover cannot write under the dashboard.
    closed: bool,
}

impl Welcome {
    pub(crate) fn start(
        interactive: bool,
        theme: Theme,
        ui: Ui,
        project: &Path,
        mode: Mode,
        log: Option<&Path>,
    ) -> Self {
        let view: Box<dyn View> = if interactive {
            Box::new(TermView::new(theme))
        } else {
            Box::new(PlainView { ui })
        };
        let mut welcome = Self {
            view,
            steps: mode
                .steps()
                .iter()
                .map(|id| Step {
                    id: *id,
                    state: StepState::Pending,
                    detail: String::new(),
                    started: None,
                    elapsed: None,
                })
                .collect(),
            log: log.and_then(|path| File::create(path).ok()),
            spinner: 0,
            unicode: theme.supports_unicode(),
            closed: false,
        };
        let width = welcome.view.width();
        let name = project
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project");
        welcome.view.open(
            &brand::render(interactive, width, theme),
            &header_line(width, env!("CARGO_PKG_VERSION"), name, mode),
        );
        welcome.record(&format!(
            "phoxal {} starting {} ({})",
            env!("CARGO_PKG_VERSION"),
            project.display(),
            mode.label()
        ));
        welcome
    }

    fn index(&self, id: StepId) -> Option<usize> {
        self.steps.iter().position(|step| step.id == id)
    }

    fn state(&self, id: StepId) -> Option<StepState> {
        self.index(id).map(|index| self.steps[index].state)
    }

    /// Whether this step has already reached a terminal state.
    pub(crate) fn settled(&self, id: StepId) -> bool {
        matches!(
            self.state(id),
            Some(StepState::Done | StepState::Failed) | None
        )
    }

    pub(crate) fn running(&self, id: StepId) -> bool {
        self.state(id) == Some(StepState::Active)
    }

    /// Close out every step before `index` that is still running. A step is
    /// finished by the next one starting: preparation has no terminal event of
    /// its own once the supervisor is being launched.
    fn settle_before(&mut self, index: usize) {
        for position in 0..index {
            if self.steps[position].state == StepState::Active {
                self.finish(position, StepState::Done, None);
            }
        }
    }

    pub(crate) fn begin(&mut self, id: StepId, detail: impl Into<String>) {
        let Some(index) = self.index(id) else {
            return;
        };
        self.settle_before(index);
        let detail = detail.into();
        let step = &mut self.steps[index];
        if step.state != StepState::Active {
            step.state = StepState::Active;
            step.started = Some(Instant::now());
        }
        step.detail = detail;
        let started = format!("step {} started: {}", id.label(), self.steps[index].detail);
        self.record(&started);
        self.draw_active();
    }

    pub(crate) fn detail(&mut self, id: StepId, detail: impl Into<String>) {
        let Some(index) = self.index(id) else {
            return;
        };
        if self.steps[index].state != StepState::Active {
            return;
        }
        self.steps[index].detail = detail.into();
        if self.view.animated() {
            self.draw_active();
        }
    }

    pub(crate) fn complete(&mut self, id: StepId, detail: impl Into<String>) {
        let Some(index) = self.index(id) else {
            return;
        };
        self.settle_before(index);
        self.finish(index, StepState::Done, Some(detail.into()));
    }

    /// Mark a step failed. `id` defaults to whichever step is running, which is
    /// what an error raised outside the checklist's own vocabulary belongs to.
    pub(crate) fn fail(&mut self, id: Option<StepId>, detail: &str) {
        let index = match id.and_then(|id| self.index(id)) {
            Some(index) => index,
            None => match self
                .steps
                .iter()
                .position(|step| step.state == StepState::Active)
            {
                Some(index) => index,
                None => return,
            },
        };
        self.finish(index, StepState::Failed, Some(detail.to_string()));
    }

    fn finish(&mut self, index: usize, state: StepState, detail: Option<String>) {
        let step = &mut self.steps[index];
        if matches!(step.state, StepState::Done | StepState::Failed) {
            return;
        }
        step.state = state;
        step.elapsed = step.started.map(|started| started.elapsed());
        if let Some(detail) = detail {
            step.detail = detail;
        }
        let line = self.line(index, self.marker(state));
        self.record(&line);
        if self.closed {
            return;
        }
        self.view.clear_active();
        match state {
            StepState::Failed => self.view.failed(&line),
            _ => self.view.done(&line),
        }
        self.draw_active();
    }

    /// One permanent line that is not a step: a preparation warning, or an
    /// error the run recovered enough to keep going past.
    pub(crate) fn note(&mut self, message: &str) {
        self.record(message);
        if self.closed {
            return;
        }
        let width = self.view.width();
        let line = truncate(
            &format!("  !  {}", sanitize(message)),
            width.saturating_sub(1),
        );
        self.view.clear_active();
        self.view.note(&line);
        self.draw_active();
    }

    /// Append one line to the startup log without showing it. Raw dependency
    /// output belongs here: it is what a failed startup is diagnosed from, and
    /// it would otherwise bury the checklist under a wall of cargo text.
    pub(crate) fn record(&mut self, line: &str) {
        if let Some(log) = self.log.as_mut() {
            let _ = writeln!(log, "{}", line.trim_end());
        }
    }

    pub(crate) fn tick(&mut self) {
        if self.closed || !self.view.animated() {
            return;
        }
        self.spinner = self.spinner.wrapping_add(1);
        self.draw_active();
    }

    /// Release the terminal: drop the transient line and restore the cursor.
    /// Every path out of a startup goes through here before anything else may
    /// own the terminal.
    pub(crate) fn close(&mut self) {
        if self.closed {
            return;
        }
        self.view.clear_active();
        self.view.close();
        self.closed = true;
    }

    /// The block a failed startup ends on: the reason, then where to read the
    /// whole story. Only logs that were actually written are named.
    pub(crate) fn report_failure(&mut self, reason: &str, logs: &[PathBuf]) {
        self.fail(None, reason.lines().next().unwrap_or(reason));
        self.view.clear_active();
        self.view.plain("");
        for line in reason.lines() {
            let line = format!("  {}", sanitize(line));
            self.record(&line);
            self.view.plain(&line);
        }
        for (index, log) in logs.iter().filter(|log| log.exists()).enumerate() {
            let label = if index == 0 { "logs" } else { "    " };
            self.view.plain(&format!("  {label} {}", log.display()));
        }
        self.close();
    }

    pub(crate) fn report_cancelled(&mut self) {
        self.view.clear_active();
        self.record("cancelled");
        self.view.plain("  cancelled");
        self.close();
    }

    fn draw_active(&mut self) {
        if self.closed || !self.view.animated() {
            return;
        }
        let Some(index) = self
            .steps
            .iter()
            .position(|step| step.state == StepState::Active)
        else {
            self.view.clear_active();
            return;
        };
        let marker = self.spinner_marker();
        let line = self.line(index, marker);
        self.view.active(&line);
    }

    const fn marker(&self, state: StepState) -> char {
        match (state, self.unicode) {
            (StepState::Failed, true) => '✗',
            (StepState::Failed, false) => 'x',
            (_, true) => '✓',
            (_, false) => '+',
        }
    }

    fn spinner_marker(&self) -> char {
        let frames = if self.unicode {
            SPINNER_UNICODE
        } else {
            SPINNER_ASCII
        };
        frames[self.spinner % frames.len()]
    }

    fn line(&self, index: usize, marker: char) -> String {
        let step = &self.steps[index];
        truncate(
            &step_line(step.id.label(), marker, &step.detail, step.elapsed()),
            self.view.width().saturating_sub(1),
        )
    }
}

impl Drop for Welcome {
    fn drop(&mut self) {
        self.close();
    }
}

fn sanitize(value: &str) -> String {
    phoxal_cli_observation::sanitize_terminal_text(value)
}

fn truncate(line: &str, width: usize) -> String {
    console::truncate_str(line, width.max(1), "…").into_owned()
}

fn step_line(label: &str, marker: char, detail: &str, elapsed: Option<Duration>) -> String {
    let detail = console::truncate_str(&sanitize(detail), DETAIL_WIDTH, "…").into_owned();
    let elapsed = elapsed.map_or_else(String::new, |elapsed| {
        format!("{:>7.1}s", elapsed.as_secs_f64())
    });
    format!("  {marker}  {label:<LABEL_WIDTH$} {detail:<DETAIL_WIDTH$} {elapsed}")
        .trim_end()
        .to_string()
}

fn header_line(width: usize, version: &str, name: &str, mode: Mode) -> String {
    let left = format!("  phoxal {version}");
    let right = format!("{} · {}", sanitize(name), mode.label());
    let available = width.saturating_sub(1).max(1);
    let left_width = console::measure_text_width(&left);
    let right_width = console::measure_text_width(&right);
    if left_width + right_width < available {
        format!(
            "{left}{:gap$}{right}",
            "",
            gap = available - left_width - right_width
        )
    } else {
        truncate(&format!("{left} {right}"), available)
    }
}

// ---------------------------------------------------------------------------
// views
// ---------------------------------------------------------------------------

trait View: Send {
    fn open(&mut self, brand: &str, header: &str);
    fn done(&mut self, line: &str);
    fn failed(&mut self, line: &str);
    fn note(&mut self, line: &str);
    /// An undecorated line: the failure block, the log pointer, blank spacing.
    fn plain(&mut self, line: &str);
    fn active(&mut self, line: &str);
    fn clear_active(&mut self);
    /// Whether this view rewrites a line in place. A piped stream does not.
    fn animated(&self) -> bool;
    fn width(&self) -> usize;
    fn close(&mut self);
}

/// Hide the cursor while a line is being rewritten in place, and restore it on
/// every exit path - including a panic, which unwinds through this drop.
struct CursorGuard {
    term: Term,
}

impl CursorGuard {
    fn new(term: Term) -> Self {
        let _ = term.hide_cursor();
        Self { term }
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        let _ = self.term.show_cursor();
        let _ = self.term.flush();
    }
}

struct TermView {
    term: Term,
    theme: Theme,
    _cursor: CursorGuard,
    active_drawn: bool,
}

impl TermView {
    fn new(theme: Theme) -> Self {
        let term = Term::stderr();
        let cursor = CursorGuard::new(term.clone());
        Self {
            term,
            theme,
            _cursor: cursor,
            active_drawn: false,
        }
    }
}

impl View for TermView {
    fn open(&mut self, brand: &str, header: &str) {
        let _ = self.term.write_line(brand);
        let _ = self.term.write_line("");
        let _ = self.term.write_line(header);
        let _ = self.term.write_line("");
    }

    fn done(&mut self, line: &str) {
        let _ = self.term.write_line(&self.theme.success(line));
    }

    fn failed(&mut self, line: &str) {
        let _ = self.term.write_line(&self.theme.error(line));
    }

    fn note(&mut self, line: &str) {
        let _ = self.term.write_line(&self.theme.warn(line));
    }

    fn plain(&mut self, line: &str) {
        let _ = self.term.write_line(line);
    }

    fn active(&mut self, line: &str) {
        let _ = self.term.clear_line();
        let _ = self.term.write_str(&self.theme.accent(line));
        let _ = self.term.flush();
        self.active_drawn = true;
    }

    fn clear_active(&mut self) {
        if self.active_drawn {
            let _ = self.term.clear_line();
            self.active_drawn = false;
        }
    }

    fn animated(&self) -> bool {
        true
    }

    fn width(&self) -> usize {
        usize::from(self.term.size().1)
    }

    fn close(&mut self) {
        let _ = self.term.show_cursor();
        let _ = self.term.flush();
    }
}

struct PlainView {
    ui: Ui,
}

impl View for PlainView {
    fn open(&mut self, _brand: &str, header: &str) {
        self.ui.info(brand::FALLBACK);
        self.ui.info(header.trim());
    }

    fn done(&mut self, line: &str) {
        self.ui.success(line.trim());
    }

    fn failed(&mut self, line: &str) {
        self.ui.error(line.trim());
    }

    fn note(&mut self, line: &str) {
        self.ui.warn(line.trim());
    }

    /// Kept un-trimmed on the left: this is the failure block, whose indent is
    /// what keeps a multi-line log list aligned under its label.
    fn plain(&mut self, line: &str) {
        if !line.trim().is_empty() {
            self.ui.info(line.trim_end());
        }
    }

    fn active(&mut self, line: &str) {
        self.ui.info(line.trim());
    }

    fn clear_active(&mut self) {}

    fn animated(&self) -> bool {
        false
    }

    /// A piped stream has no width; this is only the conventional one the
    /// header is laid out against, and it is wide enough that no step line is
    /// ever cut short in a log.
    fn width(&self) -> usize {
        80
    }

    fn close(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_ui::ColorCapability;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl View for Recorder {
        fn open(&mut self, _brand: &str, header: &str) {
            self.0.lock().unwrap().push(format!("header {header}"));
        }
        fn done(&mut self, line: &str) {
            self.0.lock().unwrap().push(format!("done {line}"));
        }
        fn failed(&mut self, line: &str) {
            self.0.lock().unwrap().push(format!("failed {line}"));
        }
        fn note(&mut self, line: &str) {
            self.0.lock().unwrap().push(format!("note {line}"));
        }
        fn plain(&mut self, line: &str) {
            self.0.lock().unwrap().push(format!("plain {line}"));
        }
        fn active(&mut self, line: &str) {
            self.0.lock().unwrap().push(format!("active {line}"));
        }
        fn clear_active(&mut self) {}
        fn animated(&self) -> bool {
            true
        }
        fn width(&self) -> usize {
            80
        }
        fn close(&mut self) {}
    }

    fn welcome(mode: Mode) -> (Welcome, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let welcome = Welcome {
            view: Box::new(Recorder(Arc::clone(&lines))),
            steps: mode
                .steps()
                .iter()
                .map(|id| Step {
                    id: *id,
                    state: StepState::Pending,
                    detail: String::new(),
                    started: None,
                    elapsed: None,
                })
                .collect(),
            log: None,
            spinner: 0,
            unicode: true,
            closed: false,
        };
        (welcome, lines)
    }

    /// Only a simulation has a simulator to bring up, and the checklist says so.
    #[test]
    fn the_simulator_step_exists_only_for_a_simulation() {
        assert!(!Mode::Native.steps().contains(&StepId::Webots));
        assert!(Mode::Webots.steps().contains(&StepId::Webots));
    }

    /// A step is finished by the next one starting, so preparation can never be
    /// left spinning behind the supervisor's own line.
    #[test]
    fn starting_a_later_step_settles_every_earlier_running_one() {
        let (mut welcome, lines) = welcome(Mode::Native);
        welcome.begin(StepId::Project, "robot.yaml");
        welcome.begin(StepId::PrepareRuntime, "cargo");
        welcome.begin(StepId::Supervisor, "launching");
        let rendered = lines.lock().unwrap().join("\n");
        assert!(rendered.contains("done   ✓  Project"), "{rendered}");
        assert!(rendered.contains("done   ✓  Prepare runtime"), "{rendered}");
        assert!(rendered.contains("active   ⠋  Supervisor"), "{rendered}");
    }

    /// The failure block names the step that failed, the reason, and the log.
    #[test]
    fn a_failed_startup_marks_the_running_step_and_points_at_the_log() {
        let (mut welcome, lines) = welcome(Mode::Webots);
        welcome.begin(StepId::Project, "robot.yaml");
        let directory = tempfile::tempdir().unwrap();
        let written = directory.path().join("startup.log");
        std::fs::write(&written, "evidence\n").unwrap();
        welcome.report_failure(
            "failed to stage Webots\nfix: reinstall Webots",
            &[written.clone(), directory.path().join("webots.log")],
        );
        let rendered = lines.lock().unwrap().join("\n");
        assert!(
            rendered.contains("failed   ✗  Project           failed to stage Webots"),
            "{rendered}"
        );
        assert!(
            rendered.contains("plain   fix: reinstall Webots"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("plain   logs {}", written.display())),
            "{rendered}"
        );
        assert!(
            !rendered.contains("webots.log"),
            "a log that was never written must not be named: {rendered}"
        );
    }

    /// Nothing may reach the terminal after the dashboard has been handed it.
    #[test]
    fn a_closed_welcome_renders_nothing_further() {
        let (mut welcome, lines) = welcome(Mode::Native);
        welcome.begin(StepId::Project, "robot.yaml");
        welcome.close();
        let before = lines.lock().unwrap().len();
        welcome.tick();
        welcome.note("a late warning");
        assert_eq!(lines.lock().unwrap().len(), before);
    }

    #[test]
    fn step_lines_are_sanitized_truncated_and_timed() {
        let line = step_line(
            "Prepare runtime",
            '✓',
            "safe\u{1b}[31m red\u{1b}[0m",
            Some(Duration::from_millis(1_250)),
        );
        assert!(!line.contains('\u{1b}'), "{line}");
        assert!(line.contains("1.2s"), "{line}");
        assert!(console::measure_text_width(&truncate(&line, 20)) <= 20);
    }

    #[test]
    fn the_header_names_the_version_the_project_and_the_mode() {
        let header = header_line(80, "0.37.7", "robot-rover", Mode::Webots);
        assert_eq!(console::measure_text_width(&header), 79);
        assert!(header.starts_with("  phoxal 0.37.7"), "{header}");
        assert!(header.ends_with("robot-rover · webots"), "{header}");
    }

    /// A colorless, non-Unicode terminal still gets a legible checklist.
    #[test]
    fn an_ascii_terminal_falls_back_to_ascii_markers() {
        let (mut welcome, lines) = welcome(Mode::Native);
        welcome.unicode = false;
        welcome.complete(StepId::Project, "robot.yaml");
        assert!(
            lines
                .lock()
                .unwrap()
                .join("\n")
                .contains("done   +  Project"),
            "{:?}",
            lines.lock().unwrap()
        );
        assert_eq!(
            brand::render(true, 200, Theme::new(ColorCapability::None)),
            brand::render(true, 200, Theme::new(ColorCapability::None))
        );
    }
}
