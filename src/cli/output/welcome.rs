#![deny(clippy::print_stdout)]

use std::collections::HashSet;
use std::io;
use std::path::Path;

use console::Term;
use phoxal_cli_core::runtime::{StartupStep, StartupStepKind, StartupStepState};
use phoxal_cli_protocol::SupervisorSnapshotV0;
use phoxal_cli_ui::Theme;

use super::brand;
use super::plain::Ui;
use crate::application::readiness::StartupPresenter;

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub(crate) fn presenter(
    interactive: bool,
    theme: Theme,
    ui: Ui,
    project: &Path,
) -> Box<dyn StartupPresenter> {
    if interactive {
        Box::new(WelcomePresenter::new(theme, project))
    } else {
        Box::new(PlainPresenter::new(ui, project))
    }
}

fn step_label(kind: StartupStepKind) -> &'static str {
    match kind {
        StartupStepKind::Project => "Project",
        StartupStepKind::PrepareRuntime => "Prepare runtime",
        StartupStepKind::Infrastructure => "Infrastructure",
        StartupStepKind::Graph => "Robot graph",
    }
}

fn safe_detail(step: &StartupStep) -> String {
    phoxal_cli_observation::sanitize_terminal_text(step.detail.as_deref().unwrap_or_default())
}

fn plain_step_line(step: &StartupStep, marker: char) -> String {
    let detail = console::truncate_str(&safe_detail(step), 36, "…").into_owned();
    let elapsed = step
        .elapsed_ms
        .map(|elapsed| format!("{:>7.1}s", elapsed as f64 / 1_000.0))
        .unwrap_or_default();
    format!(
        "  {marker}  {:<17} {:<36} {elapsed}",
        step_label(step.kind),
        detail
    )
    .trim_end()
    .to_string()
}

fn truncated_line(step: &StartupStep, marker: char, width: usize) -> String {
    console::truncate_str(
        &plain_step_line(step, marker),
        width.saturating_sub(1).max(1),
        "…",
    )
    .into_owned()
}

fn header_line(width: usize, version: &str, name: &str) -> String {
    let left = format!("  phoxal-cli {version}");
    let right = format!(
        "{} · native",
        phoxal_cli_observation::sanitize_terminal_text(name)
    );
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
        console::truncate_str(&format!("{left} {right}"), available, "…").into_owned()
    }
}

fn failure_lines(reason: &str) -> Vec<String> {
    reason
        .lines()
        .map(phoxal_cli_observation::sanitize_terminal_text)
        .collect()
}

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

pub(crate) struct WelcomePresenter {
    term: Term,
    _cursor: CursorGuard,
    theme: Theme,
    printed: HashSet<StartupStepKind>,
    latest: Option<SupervisorSnapshotV0>,
    spinner: usize,
    active_drawn: bool,
}

impl WelcomePresenter {
    fn new(theme: Theme, project: &Path) -> Self {
        let term = Term::stderr();
        let cursor = CursorGuard::new(term.clone());
        let width = usize::from(term.size().1);
        let _ = term.write_line(&brand::render(true, width, theme));
        let _ = term.write_line("");
        let name = project
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project");
        let _ = term.write_line(&header_line(width, env!("CARGO_PKG_VERSION"), name));
        let _ = term.write_line("");
        Self {
            term,
            _cursor: cursor,
            theme,
            printed: HashSet::new(),
            latest: None,
            spinner: 0,
            active_drawn: false,
        }
    }

    fn width(&self) -> usize {
        usize::from(self.term.size().1)
    }

    fn clear_active(&mut self) -> io::Result<()> {
        if self.active_drawn {
            self.term.clear_line()?;
            self.active_drawn = false;
        }
        Ok(())
    }

    fn draw(&mut self) {
        let Some(snapshot) = self.latest.clone() else {
            return;
        };
        let terminal = snapshot
            .startup
            .steps
            .iter()
            .filter(|step| {
                matches!(
                    step.state,
                    StartupStepState::Done | StartupStepState::Failed
                ) && !self.printed.contains(&step.kind)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !terminal.is_empty() {
            let _ = self.clear_active();
            for step in terminal {
                let marker = if step.state == StartupStepState::Done {
                    '✓'
                } else {
                    '✗'
                };
                let line = truncated_line(&step, marker, self.width());
                let line = if marker == '✓' {
                    self.theme.success(&line)
                } else {
                    self.theme.error(&line)
                };
                let _ = self.term.write_line(&line);
                self.printed.insert(step.kind);
            }
        }
        if let Some(active) = snapshot
            .startup
            .steps
            .iter()
            .find(|step| step.state == StartupStepState::Active)
        {
            let _ = self.term.clear_line();
            let line = truncated_line(active, SPINNER[self.spinner % SPINNER.len()], self.width());
            let _ = self.term.write_str(&self.theme.accent(&line));
            let _ = self.term.flush();
            self.active_drawn = true;
        }
    }

    fn finalize_active_failure(&mut self, reason: Option<&str>) {
        let Some(mut active) = self.latest.as_ref().and_then(|snapshot| {
            snapshot
                .startup
                .steps
                .iter()
                .find(|step| step.state == StartupStepState::Active)
                .cloned()
        }) else {
            return;
        };
        let _ = self.clear_active();
        active.state = StartupStepState::Failed;
        if let Some(reason) = reason {
            active.detail = reason.lines().next().map(ToString::to_string);
        }
        let line = self
            .theme
            .error(&truncated_line(&active, '✗', self.width()));
        let _ = self.term.write_line(&line);
        self.printed.insert(active.kind);
    }
}

impl StartupPresenter for WelcomePresenter {
    fn snapshot(&mut self, snapshot: &SupervisorSnapshotV0) {
        self.latest = Some(snapshot.clone());
        self.draw();
    }

    fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        self.draw();
    }

    fn ready(&mut self) {
        let _ = self.clear_active();
        let _ = self.term.flush();
    }

    fn cancelled(&mut self) {
        let _ = self.clear_active();
        let _ = self.term.write_line("  stopping robot…");
        let _ = self.term.flush();
    }

    fn failed(&mut self, reason: Option<&str>, log: &Path) {
        self.finalize_active_failure(reason);
        let _ = self.clear_active();
        let _ = self.term.write_line("");
        if let Some(reason) = reason {
            for line in failure_lines(reason) {
                let _ = self.term.write_line(&format!("  {line}"));
            }
            let _ = self.term.write_line("");
        }
        let _ = self
            .term
            .write_line(&format!("  full log {}", log.display()));
        let _ = self.term.flush();
    }
}

impl Drop for WelcomePresenter {
    fn drop(&mut self) {
        let _ = self.clear_active();
    }
}

struct PlainPresenter {
    ui: Ui,
    seen: Vec<(StartupStepKind, StartupStepState, Option<String>)>,
    terminal: HashSet<StartupStepKind>,
    active: Option<StartupStep>,
}

impl PlainPresenter {
    fn new(ui: Ui, project: &Path) -> Self {
        ui.info(brand::FALLBACK);
        ui.info(format!(
            "phoxal-cli {} · {}",
            env!("CARGO_PKG_VERSION"),
            project.display()
        ));
        Self {
            ui,
            seen: Vec::new(),
            terminal: HashSet::new(),
            active: None,
        }
    }
}

impl StartupPresenter for PlainPresenter {
    fn snapshot(&mut self, snapshot: &SupervisorSnapshotV0) {
        for step in &snapshot.startup.steps {
            if self.terminal.contains(&step.kind) {
                continue;
            }
            let state = (step.kind, step.state, step.detail.clone());
            if self.seen.contains(&state) || step.state == StartupStepState::Pending {
                continue;
            }
            self.seen.retain(|(kind, _, _)| *kind != step.kind);
            self.seen.push(state);
            let marker = match step.state {
                StartupStepState::Done => '✓',
                StartupStepState::Failed => '✗',
                StartupStepState::Active => '•',
                StartupStepState::Pending => continue,
            };
            let line = plain_step_line(step, marker);
            match step.state {
                StartupStepState::Done => {
                    self.active = None;
                    self.terminal.insert(step.kind);
                    self.ui.success(line);
                }
                StartupStepState::Failed => {
                    self.active = None;
                    self.terminal.insert(step.kind);
                    self.ui.error(line);
                }
                _ => {
                    self.active = Some(step.clone());
                    self.ui.info(line);
                }
            }
        }
    }

    fn cancelled(&mut self) {
        self.ui.info("stopping robot…");
    }

    fn failed(&mut self, reason: Option<&str>, log: &Path) {
        if let Some(mut active) = self.active.take()
            && !self.terminal.contains(&active.kind)
        {
            active.state = StartupStepState::Failed;
            if let Some(reason) = reason {
                active.detail = reason.lines().next().map(ToString::to_string);
            }
            self.terminal.insert(active.kind);
            self.ui.error(plain_step_line(&active, '✗'));
        }
        if let Some(reason) = reason {
            self.ui.error(reason);
        }
        self.ui.info(format!("full log {}", log.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(state: StartupStepState, detail: &str) -> StartupStep {
        StartupStep {
            kind: StartupStepKind::PrepareRuntime,
            state,
            detail: Some(detail.to_string()),
            elapsed_ms: Some(1_250),
        }
    }

    #[test]
    fn step_lines_are_sanitized_truncated_and_timed() {
        let line = plain_step_line(
            &step(StartupStepState::Done, "safe\u{1b}[31m red\u{1b}[0m"),
            '✓',
        );
        assert!(!line.contains('\u{1b}'));
        assert!(line.contains("1.2s"));
        assert!(
            console::measure_text_width(&truncated_line(
                &step(StartupStepState::Active, "long detail"),
                '⠋',
                20
            )) <= 19
        );
    }

    #[test]
    fn catalog_floor_failure_keeps_the_fix_on_its_own_line() {
        let lines = failure_lines(
            "framework 0.41.2 is too old for this phoxal-cli (needs 0.44.0+)\nfix: run `cargo update -p phoxal`, then run again",
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1],
            "fix: run `cargo update -p phoxal`, then run again"
        );
    }

    #[test]
    fn header_right_aligns_without_using_the_last_terminal_column() {
        let header = header_line(80, "0.29.4", "robot-v1");
        assert_eq!(console::measure_text_width(&header), 79);
        assert!(header.starts_with("  phoxal-cli 0.29.4"));
        assert!(header.ends_with("robot-v1 · native"));
    }
}
