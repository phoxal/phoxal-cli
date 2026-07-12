//! The identity header (one compact line) and the optional `--welcome` card.
//!
//! Both are pure presentation over data the CLI already computes elsewhere
//! ([`crate::commands::version_summary`] for the CLI's own identity,
//! [`crate::resolver::discover_robot_yaml`]/[`crate::resolver::load_robot`]
//! for the robot's). Gating lives beside
//! [`crate::update_notice::NoticePolicy`] in [`crate::commands::dispatch`]:
//! [`IdentityPolicy::allowed`] is the single suppression rule, deliberately
//! independent of `--plain` (a plain run still gets the one-line identity
//! banner - `--plain` only turns off redraw/spinner-style decoration, see
//! [`crate::output_mode`]).

use std::path::Path;

use crate::commands::MessageFormat;
use crate::resolver::{discover_robot_yaml, load_robot};
use crate::theme::Theme;

/// Robot-side identity facts shown in the compact line and the welcome card.
/// Best-effort: a project with no discoverable/parseable `robot.yaml` (e.g.
/// `doctor` run outside a robot project) simply yields `None` from
/// [`Self::discover`] rather than failing the command over decoration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentitySummary {
    pub robot: String,
    pub channel: String,
    pub manifest: String,
}

impl IdentitySummary {
    #[must_use]
    pub fn discover(project_root: &Path) -> Option<Self> {
        let manifest_path = discover_robot_yaml(project_root).ok()?;
        let robot = load_robot(&manifest_path).ok()?;
        let manifest = pathdiff::diff_paths(&manifest_path, project_root)
            .unwrap_or(manifest_path)
            .display()
            .to_string();
        Some(Self {
            robot: robot.robot.id,
            channel: robot.artifacts.channel.as_str().to_string(),
            manifest: format!("./{manifest}"),
        })
    }
}

/// The suppression rule for both the compact identity line and `--welcome`.
/// Deliberately does **not** include `--plain`: identity is one line of
/// context, not a redraw, so it survives `--plain` the same way a `Ui::info`
/// line does. It is suppressed by:
/// - `--message-format json` (identity is not part of the JSON contract),
/// - `--quiet`,
/// - a non-TTY stderr (piped/redirected - nothing interactive is reading it),
/// - a machine verb (`version`, `logs`, `status`, `service`, `self`) whose
///   whole point is a terse, scriptable answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityPolicy {
    pub interactive: bool,
    pub quiet: bool,
    pub message_format: MessageFormat,
    pub machine_verb: bool,
    pub welcome: bool,
}

impl IdentityPolicy {
    #[must_use]
    pub fn allowed(self) -> bool {
        self.interactive
            && !self.quiet
            && self.message_format == MessageFormat::Human
            && !self.machine_verb
    }
}

/// Print the compact identity line, or the `--welcome` card, to stderr - or
/// nothing, if [`IdentityPolicy::allowed`] is false or no robot manifest is
/// discoverable. Never returns an error: decoration must never fail a
/// command.
pub fn print(policy: IdentityPolicy, project_root: &Path, theme: Theme, cli_version: &str) {
    if !policy.allowed() {
        return;
    }
    let Some(summary) = IdentitySummary::discover(project_root) else {
        return;
    };
    if policy.welcome {
        let card = render_welcome_card(&summary, theme, cli_version);
        // `IdentityPolicy::allowed` already requires an interactive stderr to
        // reach this branch, so a real terminal width is available to center
        // against; a colorless/legacy terminal just gets an unindented card.
        eprintln!("{}", center_card(&card, console::Term::stderr().size().1));
    } else {
        eprintln!("{}", render_compact_line(&summary, theme));
    }
}

/// Left-pad every line of `card` by half the slack between `terminal_width`
/// and the card's own (ANSI-stripped) width, so it reads as centered rather
/// than pinned to the left margin.
fn center_card(card: &str, terminal_width: u16) -> String {
    let box_width = card
        .lines()
        .next()
        .map(|line| strip_ansi(line).chars().count())
        .unwrap_or(0);
    let margin = (terminal_width as usize)
        .saturating_sub(box_width)
        .checked_div(2)
        .unwrap_or(0);
    let indent = " ".repeat(margin);
    card.lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_compact_line(summary: &IdentitySummary, theme: Theme) -> String {
    format!(
        "{} · {} · {} · {}",
        theme.accent("phoxal"),
        summary.robot,
        summary.channel,
        theme.muted(&summary.manifest),
    )
}

/// A centered rounded card:
/// ```text
///               ╭────────────────────────────────────────────╮
///               │        ◇                                   │
///               │       ◇ ◇     p h o x a l                  │
///               │      ◇◇◇◇◇     phoxal-cli 0.9.0            │
///               │    robot      rover-01                     │
///               │    manifest   ./robot.yaml                 │
///               │    channel    dev                          │
///               ╰────────────────────────────────────────────╯
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
            Some(theme.text_primary(&summary.robot)),
        ),
        (
            "manifest".to_string(),
            Some(theme.text_primary(&summary.manifest)),
        ),
        (
            "channel".to_string(),
            Some(theme.text_primary(&summary.channel)),
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
        .map(|row| row.chars().count())
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
            .saturating_sub(plain.chars().count())
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

    #[test]
    fn center_card_indents_every_line_by_the_same_margin() {
        let card = "╭───╮\n│ x │\n╰───╯";
        let centered = center_card(card, 13);
        let lines: Vec<&str> = centered.lines().collect();
        // box width 5, terminal 13 -> margin (13-5)/2 = 4
        assert!(lines[0].starts_with("    ╭"));
        assert_eq!(
            lines
                .iter()
                .map(|line| line.len() - line.trim_start_matches(' ').len())
                .collect::<Vec<_>>(),
            vec![4, 4, 4]
        );
    }

    #[test]
    fn center_card_never_panics_when_the_terminal_is_narrower_than_the_card() {
        let card = "╭──────────────────────╮\n│ too wide for terminal │\n╰──────────────────────╯";
        // saturating_sub must keep this from underflowing/panicking.
        let centered = center_card(card, 5);
        assert!(centered.lines().next().unwrap().starts_with('╭'));
    }

    fn policy(overrides: impl FnOnce(IdentityPolicy) -> IdentityPolicy) -> IdentityPolicy {
        overrides(IdentityPolicy {
            interactive: true,
            quiet: false,
            message_format: MessageFormat::Human,
            machine_verb: false,
            welcome: false,
        })
    }

    #[test]
    fn identity_is_allowed_for_a_default_interactive_human_run() {
        assert!(policy(|policy| policy).allowed());
    }

    #[test]
    fn identity_is_suppressed_by_json_quiet_non_tty_or_machine_verb() {
        assert!(
            !policy(|policy| IdentityPolicy {
                message_format: MessageFormat::Json,
                ..policy
            })
            .allowed()
        );
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
        // changes `OutputMode` (progress drawing), never this gate.
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
    fn welcome_card_is_a_closed_rounded_box_containing_every_field() {
        let summary = IdentitySummary {
            robot: "rover-01".to_string(),
            channel: "stable".to_string(),
            manifest: "./robot.yaml".to_string(),
        };
        let theme = Theme::new(crate::theme::ColorCapability::None);
        let card = render_welcome_card(&summary, theme, "0.9.0");
        assert!(card.contains("rover-01"));
        assert!(card.contains("./robot.yaml"));
        assert!(card.contains("stable"));
        assert!(card.contains("phoxal-cli 0.9.0"));
        assert!(card.starts_with('╭'));
        assert!(card.trim_end().ends_with('╯'));
        let lines: Vec<&str> = card.lines().collect();
        let width = lines[0].chars().count();
        for line in &lines {
            assert_eq!(
                line.chars().count(),
                width,
                "every card row must share the box width: {line:?}"
            );
        }
    }
}
