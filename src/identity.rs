//! The welcome card: the CLI's identity banner, printed left-aligned once
//! whenever the human UI is permitted (Product decision 4 - there is no
//! `--welcome` flag; the card is simply the default human rendering, never
//! opt-in).
//!
//! Pure presentation over data the CLI already computes elsewhere
//! ([`crate::commands::version_summary`] for the CLI's own identity,
//! [`crate::resolver::discover_robot_yaml`]/[`crate::resolver::load_robot`]
//! for the robot's). Gating lives beside
//! [`crate::update_notice::NoticePolicy`] in [`crate::commands::dispatch`]:
//! [`IdentityPolicy::allowed`] is the single suppression rule, deliberately
//! independent of `--plain` for finite commands. Interactive foreground
//! sessions reject `--plain` because they always use the full-screen TUI.
//! A `run`/`simulation run` session under a real TTY renders the SAME card
//! inside its own TUI startup frame instead
//! (`crate::tui::render::draw_startup`); `dispatch` skips this module's
//! `print` for that case (see
//! `crate::commands::RootCommand::enters_interactive_session`) so the card
//! never appears twice.

use std::path::Path;

use unicode_width::UnicodeWidthStr;

use crate::resolver::{discover_robot_yaml, load_robot};
use crate::stores::log_store::sanitize_terminal_text;
use crate::theme::Theme;

/// User-facing shape of this invocation, shown in the welcome card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalMode {
    /// Full-screen interactive session.
    Full,
    /// Read-only preview such as `simulation run --dry-run`.
    PlanOnly,
    /// A finite command that does not own a live session.
    OneShot,
}

impl TerminalMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::PlanOnly => "plan only",
            Self::OneShot => "one shot",
        }
    }
}

/// Robot-side identity facts shown in the compact line and the welcome card.
/// Best-effort: a project with no discoverable/parseable `robot.yaml` (e.g.
/// `doctor` run outside a robot project) simply yields `None` from
/// [`Self::discover`] rather than failing the command over decoration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentitySummary {
    pub robot: String,
    pub namespace: String,
    pub channel: String,
    pub manifest: String,
    pub terminal: TerminalMode,
}

impl IdentitySummary {
    #[must_use]
    pub fn discover(project_root: &Path, terminal: TerminalMode) -> Option<Self> {
        let manifest_path = discover_robot_yaml(project_root).ok()?;
        let robot = load_robot(&manifest_path).ok()?;
        let manifest = display_manifest_path(&manifest_path, project_root);
        Some(Self {
            robot: robot.robot.id,
            namespace: robot.robot.namespace,
            channel: robot.artifacts.channel.as_str().to_string(),
            manifest,
            terminal,
        })
    }
}

fn display_manifest_path(manifest_path: &Path, project_root: &Path) -> String {
    let display_path = pathdiff::diff_paths(manifest_path, project_root)
        .unwrap_or_else(|| manifest_path.to_path_buf());
    let text = display_path.display().to_string();
    let is_local_relative = display_path.is_relative()
        && !matches!(
            display_path.components().next(),
            Some(std::path::Component::ParentDir)
        );
    if is_local_relative {
        format!("./{text}")
    } else {
        text
    }
}

/// The suppression rule for the welcome card. Deliberately does **not**
/// include `--plain`: the card is decoration shown once at command start, not
/// a redraw, so it survives `--plain` for finite commands. It is suppressed by:
/// - `--quiet`,
/// - a non-TTY stderr (piped/redirected - nothing interactive is reading it),
/// - a machine verb (`version`, `logs`, `status`, `service`, `self`) whose
///   whole point is a terse, scriptable answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityPolicy {
    pub interactive: bool,
    pub quiet: bool,
    pub machine_verb: bool,
}

impl IdentityPolicy {
    #[must_use]
    pub fn allowed(self) -> bool {
        self.interactive && !self.quiet && !self.machine_verb
    }
}

/// Print the left-aligned welcome card to stderr - or nothing, if
/// [`IdentityPolicy::allowed`] is false or no robot manifest is discoverable.
/// Never returns an error: decoration must never fail a command.
pub fn print(
    policy: IdentityPolicy,
    project_root: &Path,
    terminal: TerminalMode,
    theme: Theme,
    cli_version: &str,
) {
    if !policy.allowed() {
        return;
    }
    let Some(summary) = IdentitySummary::discover(project_root, terminal) else {
        return;
    };
    eprintln!("{}", render_welcome_card(&summary, theme, cli_version));
}

/// A left-aligned rounded card:
/// ```text
/// ╭────────────────────────────────────────────╮
/// │        ◇                                   │
/// │       ◇ ◇     p h o x a l                  │
/// │      ◇◇◇◇◇     phoxal-cli 0.9.0            │
/// │    robot      rover-01                     │
/// │    namespace  dev                          │
/// │    manifest   ./robot.yaml                 │
/// │    channel    dev                          │
/// ╰────────────────────────────────────────────╯
/// ```
fn render_welcome_card(summary: &IdentitySummary, theme: Theme, cli_version: &str) -> String {
    use crate::theme::box_style::{
        BOTTOM_LEFT, BOTTOM_RIGHT, HORIZONTAL, TOP_LEFT, TOP_RIGHT, VERTICAL,
    };

    let mark = [" ◇", "◇ ◇", "◇◇◇◇◇"];
    let title = "p h o x a l";
    let version_line = format!("phoxal-cli {cli_version}");
    let rows: Vec<(String, Option<String>)> = vec![
        (format!("  {}", mark[0]), None),
        (format!(" {}   {}", mark[1], theme.bold(title)), None),
        (
            format!("{}    {}", mark[2], theme.muted(&version_line)),
            None,
        ),
        (String::new(), None),
        (
            "robot".to_string(),
            Some(theme.text_primary(&sanitize_terminal_text(&summary.robot))),
        ),
        (
            "namespace".to_string(),
            Some(theme.text_primary(&sanitize_terminal_text(&summary.namespace))),
        ),
        (
            "manifest".to_string(),
            Some(theme.text_primary(&sanitize_terminal_text(&summary.manifest))),
        ),
        (
            "channel".to_string(),
            Some(theme.text_primary(&sanitize_terminal_text(&summary.channel))),
        ),
        (
            "terminal".to_string(),
            Some(theme.text_primary(summary.terminal.label())),
        ),
    ];

    // Width is measured on the plain (unstyled) text so ANSI escapes never
    // throw off alignment; every row is padded to the widest plain line.
    let plain_rows: Vec<String> = rows
        .iter()
        .map(|(label, value)| match value {
            Some(value) => format!("{label:<10}{}", strip_ansi(value)),
            None => strip_ansi(label),
        })
        .collect();
    let inner_width = plain_rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.as_str()))
        .max()
        .unwrap_or(0)
        + 4;

    let mut card = String::new();
    card.push_str(&theme.border(&format!(
        "{TOP_LEFT}{}{TOP_RIGHT}",
        HORIZONTAL.to_string().repeat(inner_width)
    )));
    for ((label, value), plain) in rows.iter().zip(plain_rows.iter()) {
        let rendered = match value {
            Some(value) => format!("{label:<10}{value}"),
            None => label.clone(),
        };
        let pad = inner_width
            .saturating_sub(UnicodeWidthStr::width(plain.as_str()))
            .saturating_sub(2);
        card.push('\n');
        card.push_str(&theme.border(&VERTICAL.to_string()));
        card.push_str("  ");
        card.push_str(&rendered);
        card.push_str(&" ".repeat(pad));
        card.push_str(&theme.border(&VERTICAL.to_string()));
    }
    card.push('\n');
    card.push_str(&theme.border(&format!(
        "{BOTTOM_LEFT}{}{BOTTOM_RIGHT}",
        HORIZONTAL.to_string().repeat(inner_width)
    )));
    card
}

/// Strip ANSI SGR escapes for width measurement. Handles only the `CSI ... m`
/// form this module itself emits (via [`Theme`]); good enough for layout, not
/// a general ANSI parser.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(overrides: impl FnOnce(IdentityPolicy) -> IdentityPolicy) -> IdentityPolicy {
        overrides(IdentityPolicy {
            interactive: true,
            quiet: false,
            machine_verb: false,
        })
    }

    #[test]
    fn identity_is_allowed_for_a_default_interactive_run() {
        assert!(policy(|policy| policy).allowed());
    }

    #[test]
    fn identity_is_suppressed_by_quiet_non_tty_or_machine_verb() {
        assert!(
            !policy(|policy| IdentityPolicy {
                quiet: true,
                ..policy
            })
            .allowed()
        );
        assert!(
            !policy(|policy| IdentityPolicy {
                interactive: false,
                ..policy
            })
            .allowed()
        );
        assert!(
            !policy(|policy| IdentityPolicy {
                machine_verb: true,
                ..policy
            })
            .allowed()
        );
    }

    #[test]
    fn plain_is_not_part_of_the_identity_suppression_rule() {
        // `IdentityPolicy` has no `plain` field at all: `--plain` only
        // changes progress styling, never this gate.
        let policy = policy(|policy| policy);
        assert!(policy.allowed());
    }

    #[test]
    fn strip_ansi_removes_sgr_sequences_but_keeps_the_text() {
        let theme = Theme::new(crate::theme::ColorCapability::TrueColor);
        let painted = theme.accent("rover-01");
        assert_eq!(strip_ansi(&painted), "rover-01");
    }

    #[test]
    fn manifest_display_prefixes_only_paths_inside_the_project() {
        let project = Path::new("/workspace/robot");
        assert_eq!(
            display_manifest_path(Path::new("/workspace/robot/robot.yaml"), project),
            "./robot.yaml"
        );
        assert_eq!(
            display_manifest_path(Path::new("/workspace/robot/../robot.yaml"), project),
            "../robot.yaml"
        );
        assert_eq!(
            display_manifest_path(Path::new("/outside/robot.yaml"), Path::new("relative/root")),
            "/outside/robot.yaml"
        );
    }

    #[test]
    fn welcome_card_is_a_closed_rounded_box_containing_every_field() {
        let summary = IdentitySummary {
            robot: "探査-rover-01".to_string(),
            namespace: "dev".to_string(),
            channel: "stable".to_string(),
            manifest: "./robot.yaml".to_string(),
            terminal: TerminalMode::Full,
        };
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let card = render_welcome_card(&summary, theme, "0.9.0");
        assert!(card.contains("探査-rover-01"));
        assert!(card.contains("dev"));
        assert!(card.contains("./robot.yaml"));
        assert!(card.contains("stable"));
        assert!(card.contains("full"));
        assert!(card.contains("phoxal-cli 0.9.0"));
        assert!(card.starts_with('╭'));
        assert!(card.trim_end().ends_with('╯'));
        let lines: Vec<&str> = card.lines().collect();
        let width = UnicodeWidthStr::width(lines[0]);
        for line in &lines {
            assert_eq!(
                UnicodeWidthStr::width(*line),
                width,
                "every card row must share the box width: {line:?}"
            );
        }
    }
}
