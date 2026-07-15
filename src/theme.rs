//! The shared visual theme: the locked brand palette, terminal color-capability
//! detection, and state/resource styling helpers.
//!
//! This is the single source of color/style for the CLI. No other call site
//! hard-codes a color after this: [`crate::ui::Ui`] routes every message
//! through [`Theme`], the progress primitive ([`crate::progress`]) styles its
//! spinner/bar through it, and the identity header/welcome card
//! ([`crate::identity`]) uses it for the box and accents. A later full-screen
//! TUI reuses the same palette, state styling, and non-color symbols so the
//! rendering stays one system end to end.
//!
//! Every role degrades deliberately across four capability tiers
//! ([`ColorCapability`]): truecolor, 256-color, basic 16-color, and colorless.
//! Selection/state is never color-only - [`state_symbol`] and
//! [`ParticipantState::label`](crate::supervisor::ParticipantState::label)
//! give every state a marker and a word that survive `--plain`/`NO_COLOR`.

use std::io::IsTerminal;

use console::Style;

use crate::output_mode::OutputMode;

use crate::supervisor::ParticipantState;

/// A brand color in its canonical truecolor form. Every degraded
/// representation ([`ColorCapability::Ansi256`], [`ColorCapability::Ansi16`])
/// is derived from this at render time, never hand-picked separately, so the
/// palette table below is the only place a color is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// The locked brand palette (docs: CLI UX overhaul, Phase 0 foundation).
pub mod palette {
    use super::Rgb;

    pub const BG: Rgb = Rgb(0x06, 0x08, 0x0b);
    pub const SURFACE: Rgb = Rgb(0x0e, 0x14, 0x1b);
    pub const SURFACE_STRONG: Rgb = Rgb(0x15, 0x1d, 0x26);
    pub const BORDER: Rgb = Rgb(0x22, 0x30, 0x3a);
    pub const TEXT: Rgb = Rgb(0xff, 0xff, 0xff);
    pub const TEXT_PRIMARY: Rgb = Rgb(0xb7, 0xc4, 0xcf);
    pub const MUTED: Rgb = Rgb(0x6e, 0x7b, 0x87);
    pub const MUTED_STRONG: Rgb = Rgb(0x8b, 0x98, 0xa4);
    pub const ACCENT: Rgb = Rgb(0x7f, 0xe7, 0xe3);
    pub const ACCENT_2: Rgb = Rgb(0x4c, 0xcb, 0xc5);
    pub const STEEL: Rgb = Rgb(0x87, 0xab, 0xc4);
    pub const SUCCESS: Rgb = Rgb(0x5f, 0xd3, 0x9c);
    pub const WARN: Rgb = Rgb(0xe3, 0xb3, 0x41);
    pub const ERROR: Rgb = Rgb(0xef, 0x53, 0x50);
}

/// A semantic color role. Call sites ask for a role, never a hex value or a
/// `console::Color`, so the palette can move in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Bg,
    Surface,
    SurfaceStrong,
    Border,
    Text,
    TextPrimary,
    Muted,
    MutedStrong,
    Accent,
    Accent2,
    Steel,
    Success,
    Warn,
    Error,
}

impl Role {
    #[must_use]
    pub const fn rgb(self) -> Rgb {
        match self {
            Self::Bg => palette::BG,
            Self::Surface => palette::SURFACE,
            Self::SurfaceStrong => palette::SURFACE_STRONG,
            Self::Border => palette::BORDER,
            Self::Text => palette::TEXT,
            Self::TextPrimary => palette::TEXT_PRIMARY,
            Self::Muted => palette::MUTED,
            Self::MutedStrong => palette::MUTED_STRONG,
            Self::Accent => palette::ACCENT,
            Self::Accent2 => palette::ACCENT_2,
            Self::Steel => palette::STEEL,
            Self::Success => palette::SUCCESS,
            Self::Warn => palette::WARN,
            Self::Error => palette::ERROR,
        }
    }
}

/// What the target terminal can render. Detected once per stream from
/// `NO_COLOR`, TTY-ness, `COLORTERM`, and `TERM`; every [`Theme`] method
/// degrades through exactly these four tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorCapability {
    TrueColor,
    Ansi256,
    Ansi16,
    None,
}

/// Detect color capability for a stream, given whether that stream is a TTY.
/// Pure function of its inputs so it is unit-testable without touching real
/// environment variables; [`Theme::detect_stderr`] is the real entry point.
#[must_use]
pub fn detect_color_capability(
    is_tty: bool,
    no_color: Option<&str>,
    colorterm: Option<&str>,
    term: Option<&str>,
) -> ColorCapability {
    // NO_COLOR (https://no-color.org/): presence of the variable (any value,
    // including empty) disables color, full stop.
    if !is_tty || no_color.is_some() {
        return ColorCapability::None;
    }
    if matches!(colorterm, Some("truecolor") | Some("24bit")) {
        return ColorCapability::TrueColor;
    }
    match term {
        Some("dumb") => ColorCapability::None,
        Some(term) if term.contains("256color") => ColorCapability::Ansi256,
        _ => ColorCapability::Ansi16,
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The shared theme: a resolved [`ColorCapability`] plus the helpers every
/// output path renders through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    capability: ColorCapability,
}

impl Theme {
    #[must_use]
    pub const fn new(capability: ColorCapability) -> Self {
        Self { capability }
    }

    /// Detect the theme for stderr, the stream every piece of decoration in
    /// this CLI (identity, progress, log lines) writes to, given the
    /// session's already-computed [`OutputMode`].
    ///
    /// `--plain`/`--quiet` (via a [`OutputMode::Plain`] mode) also force
    /// [`ColorCapability::None`] even on a genuine TTY: "plain" output should
    /// mean plain, the same way `--message-format json` forces it below.
    /// [`OutputMode::Json`] forces it too, belt-and-suspenders with
    /// [`crate::ui::Ui`]'s own json gate.
    #[must_use]
    pub fn detect_stderr(mode: OutputMode) -> Self {
        if !mode.allows_progress_drawing() {
            return Self::new(ColorCapability::None);
        }
        Self::new(detect_color_capability(
            std::io::stderr().is_terminal(),
            env_var("NO_COLOR").as_deref(),
            env_var("COLORTERM").as_deref(),
            env_var("TERM").as_deref(),
        ))
    }

    #[must_use]
    pub const fn capability(self) -> ColorCapability {
        self.capability
    }

    /// Paint `text` in `role`'s color, degraded to this theme's capability.
    /// Returns `text` unchanged under [`ColorCapability::None`].
    #[must_use]
    pub fn paint(self, role: Role, text: &str) -> String {
        match self.capability {
            ColorCapability::None => text.to_string(),
            ColorCapability::TrueColor => {
                let Rgb(r, g, b) = role.rgb();
                format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
            }
            ColorCapability::Ansi256 => Style::new()
                .color256(rgb_to_ansi256(role.rgb()))
                .force_styling(true)
                .apply_to(text)
                .to_string(),
            ColorCapability::Ansi16 => Style::new()
                .fg(rgb_to_ansi16(role.rgb()))
                .force_styling(true)
                .apply_to(text)
                .to_string(),
        }
    }

    #[must_use]
    pub fn bold(self, text: &str) -> String {
        if self.capability == ColorCapability::None {
            text.to_string()
        } else {
            Style::new()
                .bold()
                .force_styling(true)
                .apply_to(text)
                .to_string()
        }
    }

    #[must_use]
    pub fn text(self, text: &str) -> String {
        self.paint(Role::Text, text)
    }

    #[must_use]
    pub fn text_primary(self, text: &str) -> String {
        self.paint(Role::TextPrimary, text)
    }

    #[must_use]
    pub fn muted(self, text: &str) -> String {
        self.paint(Role::Muted, text)
    }

    #[must_use]
    pub fn muted_strong(self, text: &str) -> String {
        self.paint(Role::MutedStrong, text)
    }

    #[must_use]
    pub fn accent(self, text: &str) -> String {
        self.paint(Role::Accent, text)
    }

    #[must_use]
    pub fn accent2(self, text: &str) -> String {
        self.paint(Role::Accent2, text)
    }

    #[must_use]
    pub fn steel(self, text: &str) -> String {
        self.paint(Role::Steel, text)
    }

    #[must_use]
    pub fn success(self, text: &str) -> String {
        self.paint(Role::Success, text)
    }

    #[must_use]
    pub fn warn(self, text: &str) -> String {
        self.paint(Role::Warn, text)
    }

    #[must_use]
    pub fn error(self, text: &str) -> String {
        self.paint(Role::Error, text)
    }

    #[must_use]
    pub fn border(self, text: &str) -> String {
        self.paint(Role::Border, text)
    }

    /// A participant state rendered as a non-color-dependent badge (symbol +
    /// label, see [`state_symbol`]) colored by [`state_role`]. Reused as-is by
    /// a later TUI/logger so every state reads identically everywhere.
    #[must_use]
    pub fn participant_state(self, state: ParticipantState) -> String {
        let badge = format!("{} {}", state_symbol(state), state.label());
        self.paint(state_role(state), &badge)
    }

    /// The dotted color spec `indicatif` templates understand
    /// (`console::Style::from_dotted_str`), or `None` under
    /// [`ColorCapability::None`] so the caller can build a colorless
    /// template. `indicatif` templates have no truecolor syntax, so
    /// [`ColorCapability::TrueColor`] degrades one step further to the
    /// nearest 256-color index here - visually indistinguishable for a thin
    /// spinner/bar glyph.
    #[must_use]
    pub fn indicatif_tag(self, role: Role) -> Option<String> {
        match self.capability {
            ColorCapability::None => None,
            ColorCapability::TrueColor | ColorCapability::Ansi256 => {
                Some(rgb_to_ansi256(role.rgb()).to_string())
            }
            ColorCapability::Ansi16 => Some(ansi16_name(rgb_to_ansi16(role.rgb())).to_string()),
        }
    }

    /// A resource-load meter fill: `load` (0.0-1.0) of `width` block
    /// characters, colored accent -> warn -> error as load rises. The filled
    /// count is itself the non-color signal (a screen reader or a
    /// colorless terminal still sees "6 of 10 blocks filled").
    #[must_use]
    pub fn resource_meter(self, load: f64, width: usize) -> String {
        let load = load.clamp(0.0, 1.0);
        let filled = ((load * width as f64).round() as usize).min(width);
        let role = resource_role(load);
        let bar: String = "█".repeat(filled) + &"░".repeat(width - filled);
        self.paint(role, &bar)
    }
}

/// The resource-meter color band: accent under normal load, warn approaching
/// saturation, error at/over it.
#[must_use]
pub const fn resource_role(load: f64) -> Role {
    if load < 0.6 {
        Role::Accent
    } else if load < 0.85 {
        Role::Warn
    } else {
        Role::Error
    }
}

/// The color role for a participant state. Ready is success, actively
/// starting/restarting is steel (neutral-in-progress), degraded is warn,
/// failed is error; stopped is muted (no longer running, not an error).
#[must_use]
pub const fn state_role(state: ParticipantState) -> Role {
    match state {
        ParticipantState::Ready => Role::Success,
        ParticipantState::Starting | ParticipantState::Restarting => Role::Steel,
        ParticipantState::Degraded => Role::Warn,
        ParticipantState::Failed => Role::Error,
        ParticipantState::Stopped => Role::Muted,
    }
}

/// A color-independent glyph for a participant state, so state is always
/// legible without color (`--plain`, `NO_COLOR`, piped output).
#[must_use]
pub const fn state_symbol(state: ParticipantState) -> &'static str {
    match state {
        ParticipantState::Starting => "…",
        ParticipantState::Ready => "✓",
        ParticipantState::Degraded => "!",
        ParticipantState::Failed => "✗",
        ParticipantState::Restarting => "↻",
        ParticipantState::Stopped => "■",
    }
}

/// Rounded box-drawing glyphs shared by the welcome card today and any later
/// panel/TUI chrome.
pub mod box_style {
    pub const TOP_LEFT: char = '╭';
    pub const TOP_RIGHT: char = '╮';
    pub const BOTTOM_LEFT: char = '╰';
    pub const BOTTOM_RIGHT: char = '╯';
    pub const HORIZONTAL: char = '─';
    pub const VERTICAL: char = '│';
}

/// Map a truecolor role to the nearest xterm 256-color palette index: the
/// 6x6x6 color cube for chromatic colors, the 24-step grayscale ramp for
/// near-neutral ones.
#[must_use]
pub fn rgb_to_ansi256(Rgb(r, g, b): Rgb) -> u8 {
    if r == g && g == b {
        return if r < 8 {
            16
        } else if r > 248 {
            231
        } else {
            (232 + (u16::from(r) - 8) * 24 / 247) as u8
        };
    }
    let cube = |c: u8| -> u16 { (u16::from(c) * 5 + 127) / 255 };
    (16 + 36 * cube(r) + 6 * cube(g) + cube(b)) as u8
}

/// Map a truecolor role to the nearest of the 8 basic ANSI colors (by
/// Euclidean distance to the standard xterm swatch values). `console::Color`
/// has no bright/16-color variants, so this is the coarsest supported tier.
#[must_use]
pub fn rgb_to_ansi16(target: Rgb) -> console::Color {
    const BASIC: [(Rgb, console::Color); 8] = [
        (Rgb(0, 0, 0), console::Color::Black),
        (Rgb(205, 0, 0), console::Color::Red),
        (Rgb(0, 205, 0), console::Color::Green),
        (Rgb(205, 205, 0), console::Color::Yellow),
        (Rgb(0, 0, 238), console::Color::Blue),
        (Rgb(205, 0, 205), console::Color::Magenta),
        (Rgb(0, 205, 205), console::Color::Cyan),
        (Rgb(229, 229, 229), console::Color::White),
    ];
    BASIC
        .into_iter()
        .min_by_key(|(rgb, _)| rgb_distance_sq(*rgb, target))
        .map_or(console::Color::White, |(_, color)| color)
}

fn rgb_distance_sq(a: Rgb, b: Rgb) -> u32 {
    let dr = i32::from(a.0) - i32::from(b.0);
    let dg = i32::from(a.1) - i32::from(b.1);
    let db = i32::from(a.2) - i32::from(b.2);
    (dr * dr + dg * dg + db * db) as u32
}

fn ansi16_name(color: console::Color) -> &'static str {
    match color {
        console::Color::Black => "black",
        console::Color::Red => "red",
        console::Color::Green => "green",
        console::Color::Yellow => "yellow",
        console::Color::Blue => "blue",
        console::Color::Magenta => "magenta",
        console::Color::Cyan => "cyan",
        console::Color::White | console::Color::Color256(_) => "white",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_wins_over_a_true_tty_and_truecolor_env() {
        let capability =
            detect_color_capability(true, Some(""), Some("truecolor"), Some("xterm-256color"));
        assert_eq!(capability, ColorCapability::None);
    }

    #[test]
    fn non_tty_is_always_colorless_even_with_truecolor_env() {
        let capability = detect_color_capability(false, None, Some("truecolor"), None);
        assert_eq!(capability, ColorCapability::None);
    }

    #[test]
    fn colorterm_truecolor_or_24bit_selects_truecolor() {
        assert_eq!(
            detect_color_capability(true, None, Some("truecolor"), None),
            ColorCapability::TrueColor
        );
        assert_eq!(
            detect_color_capability(true, None, Some("24bit"), None),
            ColorCapability::TrueColor
        );
    }

    #[test]
    fn term_256color_selects_ansi256_without_colorterm() {
        assert_eq!(
            detect_color_capability(true, None, None, Some("xterm-256color")),
            ColorCapability::Ansi256
        );
    }

    #[test]
    fn dumb_term_is_colorless() {
        assert_eq!(
            detect_color_capability(true, None, None, Some("dumb")),
            ColorCapability::None
        );
    }

    #[test]
    fn plain_term_falls_back_to_basic_16_color() {
        assert_eq!(
            detect_color_capability(true, None, None, Some("xterm")),
            ColorCapability::Ansi16
        );
        assert_eq!(
            detect_color_capability(true, None, None, None),
            ColorCapability::Ansi16
        );
    }

    #[test]
    fn none_capability_never_emits_escape_codes() {
        let theme = Theme::new(ColorCapability::None);
        assert_eq!(theme.accent("hello"), "hello");
        assert_eq!(theme.bold("hello"), "hello");
        assert_eq!(theme.indicatif_tag(Role::Accent), None);
    }

    #[test]
    fn truecolor_theme_emits_a_24_bit_escape_sequence() {
        let theme = Theme::new(ColorCapability::TrueColor);
        let painted = theme.accent("hi");
        assert!(painted.starts_with("\x1b[38;2;127;231;227m"));
        assert!(painted.ends_with("\x1b[0mhi\x1b[0m") || painted.ends_with("hi\x1b[0m"));
        assert!(painted.contains("hi"));
    }

    #[test]
    fn ansi256_theme_emits_a_256_color_escape_and_no_raw_rgb_sequence() {
        let theme = Theme::new(ColorCapability::Ansi256);
        let painted = theme.accent("hi");
        assert!(painted.contains("hi"));
        assert!(!painted.contains("38;2;"));
    }

    #[test]
    fn ansi16_theme_never_emits_a_256_or_truecolor_sequence() {
        let theme = Theme::new(ColorCapability::Ansi16);
        let painted = theme.accent("hi");
        assert!(painted.contains("hi"));
        assert!(!painted.contains("38;2;"));
        assert!(!painted.contains("38;5;"));
    }

    #[test]
    fn pure_primaries_map_to_their_matching_basic_ansi16_color() {
        assert_eq!(rgb_to_ansi16(Rgb(255, 0, 0)), console::Color::Red);
        assert_eq!(rgb_to_ansi16(Rgb(0, 255, 0)), console::Color::Green);
        assert_eq!(rgb_to_ansi16(Rgb(0, 0, 255)), console::Color::Blue);
        assert_eq!(rgb_to_ansi16(Rgb(0, 0, 0)), console::Color::Black);
        assert_eq!(rgb_to_ansi16(Rgb(255, 255, 255)), console::Color::White);
    }

    #[test]
    fn grayscale_rgb_maps_into_the_256_grayscale_ramp() {
        assert_eq!(rgb_to_ansi256(Rgb(0, 0, 0)), 16);
        assert_eq!(rgb_to_ansi256(Rgb(255, 255, 255)), 231);
        let mid = rgb_to_ansi256(Rgb(128, 128, 128));
        assert!((232..=255).contains(&mid));
    }

    #[test]
    fn resource_meter_bands_accent_warn_and_error_by_load() {
        assert_eq!(resource_role(0.0), Role::Accent);
        assert_eq!(resource_role(0.59), Role::Accent);
        assert_eq!(resource_role(0.6), Role::Warn);
        assert_eq!(resource_role(0.84), Role::Warn);
        assert_eq!(resource_role(0.85), Role::Error);
        assert_eq!(resource_role(1.0), Role::Error);
    }

    #[test]
    fn resource_meter_fill_count_is_the_non_color_signal() {
        let theme = Theme::new(ColorCapability::None);
        let meter = theme.resource_meter(0.5, 10);
        assert_eq!(meter.chars().filter(|c| *c == '█').count(), 5);
        assert_eq!(meter.chars().filter(|c| *c == '░').count(), 5);
    }

    #[test]
    fn every_participant_state_has_a_symbol_and_a_role() {
        for state in [
            ParticipantState::Starting,
            ParticipantState::Ready,
            ParticipantState::Degraded,
            ParticipantState::Failed,
            ParticipantState::Restarting,
            ParticipantState::Stopped,
        ] {
            assert!(!state_symbol(state).is_empty());
            let _ = state_role(state);
        }
        assert_eq!(state_role(ParticipantState::Ready), Role::Success);
        assert_eq!(state_role(ParticipantState::Failed), Role::Error);
        assert_eq!(state_role(ParticipantState::Degraded), Role::Warn);
    }

    #[test]
    fn participant_state_badge_always_carries_the_label_text() {
        let theme = Theme::new(ColorCapability::None);
        assert_eq!(theme.participant_state(ParticipantState::Ready), "✓ ready");
    }
}
