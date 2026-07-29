//! Bridge [`crate::theme::Theme`]'s role-based palette into ratatui
//! [`Style`]/[`Color`], degrading through the exact same four
//! [`ColorCapability`] tiers the rest of the CLI already uses. No color is ever
//! picked directly here; every call site asks for a [`Role`] and this module
//! is the only place that turns one into a ratatui `Color`.

use tuirealm::ratatui::style::{Color, Modifier, Style};

use super::{ColorCapability, Rgb, Role, Theme, rgb_to_ansi16, rgb_to_ansi256};

/// `role`'s color at `theme`'s capability - `Color::Reset` (no color at all)
/// under [`ColorCapability::None`], matching [`Theme::paint`]'s own
/// colorless passthrough so a `NO_COLOR`/non-truecolor terminal
/// never receives an escape sequence the terminal cannot render faithfully.
#[must_use]
pub fn color(theme: Theme, role: Role) -> Color {
    match theme.capability() {
        ColorCapability::None => Color::Reset,
        ColorCapability::TrueColor => {
            let Rgb(r, g, b) = role.rgb();
            Color::Rgb(r, g, b)
        }
        ColorCapability::Ansi256 => Color::Indexed(rgb_to_ansi256(role.rgb())),
        ColorCapability::Ansi16 => console_to_ratatui(rgb_to_ansi16(role.rgb())),
    }
}

fn console_to_ratatui(value: console::Color) -> Color {
    match value {
        console::Color::Black => Color::Black,
        console::Color::Red => Color::Red,
        console::Color::Green => Color::Green,
        console::Color::Yellow => Color::Yellow,
        console::Color::Blue => Color::Blue,
        console::Color::Magenta => Color::Magenta,
        console::Color::Cyan => Color::Cyan,
        console::Color::White | console::Color::Color256(_) => Color::White,
    }
}

/// A plain foreground style in `role`'s color - the common case for text.
#[must_use]
pub fn fg(theme: Theme, role: Role) -> Style {
    Style::default().fg(color(theme, role))
}

/// `fg` plus a reversed-video background, used for the selected navigator
/// row's "strong selected-row background" (design doc): selection must read
/// even at [`ColorCapability::None`], where `fg` alone would be invisible -
/// `Modifier::REVERSED` swaps foreground/background at the terminal level
/// and survives every capability tier, including colorless.
#[must_use]
pub fn selected(theme: Theme, role: Role) -> Style {
    fg(theme, role).add_modifier(Modifier::REVERSED)
}

/// Soft focus for a candidate that arrows can move to but Enter has not yet
/// activated. This is deliberately weaker than reversed-video selection.
#[must_use]
pub fn candidate(theme: Theme, role: Role) -> Style {
    fg(theme, role).add_modifier(Modifier::DIM | Modifier::BOLD)
}

/// A dim/secondary style for muted chrome (group headings, placeholders).
#[must_use]
pub fn muted(theme: Theme) -> Style {
    fg(theme, Role::Muted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_capability_never_emits_a_color() {
        let theme = Theme::new(ColorCapability::None);
        assert_eq!(color(theme, Role::Accent), Color::Reset);
        assert_eq!(color(theme, Role::Error), Color::Reset);
    }

    #[test]
    fn truecolor_capability_yields_an_rgb_color() {
        let theme = Theme::new(ColorCapability::TrueColor);
        assert!(matches!(color(theme, Role::Accent), Color::Rgb(_, _, _)));
    }

    #[test]
    fn ansi256_capability_yields_an_indexed_color_never_rgb() {
        let theme = Theme::new(ColorCapability::Ansi256);
        assert!(matches!(color(theme, Role::Accent), Color::Indexed(_)));
    }

    #[test]
    fn ansi16_capability_never_yields_rgb_or_indexed() {
        let theme = Theme::new(ColorCapability::Ansi16);
        let painted = color(theme, Role::Accent);
        assert!(!matches!(painted, Color::Rgb(_, _, _) | Color::Indexed(_)));
    }

    #[test]
    fn selected_style_always_carries_the_reversed_modifier() {
        let theme = Theme::new(ColorCapability::None);
        let style = selected(theme, Role::Success);
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }
}
