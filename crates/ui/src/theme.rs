//! The shared visual theme: the locked brand palette, terminal color-capability
//! detection, and resource styling helpers.
//!
//! This is the single source of color/style for the CLI. No other call site
//! hard-codes a color after this. Finite output and the full-screen TUI reuse
//! the same palette so rendering stays one system end to end.
//!
//! Every role degrades deliberately across four capability tiers
//! ([`ColorCapability`]): truecolor, 256-color, basic 16-color, and colorless.
//! Selection and state remain legible without color.

use std::io::IsTerminal;

use console::Style;

pub(crate) mod role;

/// A brand color in its canonical truecolor form. Every degraded
/// representation ([`ColorCapability::Ansi256`], [`ColorCapability::Ansi16`])
/// is derived from this at render time, never hand-picked separately, so the
/// palette table below is the only place a color is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// The locked brand palette shared by every terminal view.
pub mod palette {
    use super::Rgb;

    pub const BORDER: Rgb = Rgb(0x22, 0x30, 0x3a);
    pub const MUTED: Rgb = Rgb(0x6e, 0x7b, 0x87);
    pub const ACCENT: Rgb = Rgb(0x7f, 0xe7, 0xe3);
    pub const STEEL: Rgb = Rgb(0x87, 0xab, 0xc4);
    pub const SUCCESS: Rgb = Rgb(0x5f, 0xd3, 0x9c);
    pub const WARN: Rgb = Rgb(0xe3, 0xb3, 0x41);
    pub const ERROR: Rgb = Rgb(0xef, 0x53, 0x50);
}

/// A semantic color role. Call sites ask for a role, never a hex value or a
/// `console::Color`, so the palette can move in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Border,
    Muted,
    Accent,
    Steel,
    Success,
    Warn,
    Error,
}

impl Role {
    #[must_use]
    pub const fn rgb(self) -> Rgb {
        match self {
            Self::Border => palette::BORDER,
            Self::Muted => palette::MUTED,
            Self::Accent => palette::ACCENT,
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

/// Detect whether terminal text may use Unicode glyphs independently from
/// color capability. `NO_COLOR` never implies ASCII; an explicit C/POSIX
/// locale or `TERM=dumb` does.
#[must_use]
pub fn detect_unicode_support(
    term: Option<&str>,
    lc_all: Option<&str>,
    lc_ctype: Option<&str>,
    lang: Option<&str>,
) -> bool {
    if term == Some("dumb") {
        return false;
    }
    let locale = lc_all.or(lc_ctype).or(lang);
    !matches!(locale, Some("C") | Some("POSIX"))
}

/// Resolve the current terminal's glyph capability. Kept separate from
/// [`Theme`] so colorless Unicode terminals still retain readable symbols.
#[must_use]
pub fn terminal_supports_unicode() -> bool {
    detect_unicode_support(
        env_var("TERM").as_deref(),
        env_var("LC_ALL").as_deref(),
        env_var("LC_CTYPE").as_deref(),
        env_var("LANG").as_deref(),
    )
}

/// The shared theme: a resolved [`ColorCapability`] plus the helpers every
/// output path renders through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    capability: ColorCapability,
    unicode: bool,
}

impl Theme {
    #[must_use]
    pub const fn new(capability: ColorCapability) -> Self {
        Self {
            capability,
            unicode: true,
        }
    }

    /// Detect the theme for stderr from the invocation's already-computed
    /// interactive flag. Batch output always uses [`ColorCapability::None`].
    #[must_use]
    pub fn detect_stderr(interactive: bool) -> Self {
        if !interactive {
            return Self::new(ColorCapability::None);
        }
        Self {
            capability: detect_color_capability(
                std::io::stderr().is_terminal(),
                env_var("NO_COLOR").as_deref(),
                env_var("COLORTERM").as_deref(),
                env_var("TERM").as_deref(),
            ),
            unicode: terminal_supports_unicode(),
        }
    }

    #[must_use]
    pub const fn capability(self) -> ColorCapability {
        self.capability
    }

    #[must_use]
    pub const fn supports_unicode(self) -> bool {
        self.unicode
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
    pub fn accent(self, text: &str) -> String {
        self.paint(Role::Accent, text)
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
    fn unicode_detection_is_independent_from_color_and_respects_ascii_environments() {
        assert!(detect_unicode_support(
            Some("xterm-256color"),
            None,
            None,
            Some("en_US.UTF-8")
        ));
        assert!(!detect_unicode_support(
            Some("xterm-256color"),
            Some("C"),
            None,
            Some("en_US.UTF-8")
        ));
        assert!(!detect_unicode_support(
            Some("dumb"),
            None,
            None,
            Some("en_US.UTF-8")
        ));
    }

    #[test]
    fn none_capability_never_emits_escape_codes() {
        let theme = Theme::new(ColorCapability::None);
        assert_eq!(theme.accent("hello"), "hello");
        assert_eq!(theme.bold("hello"), "hello");
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
}
