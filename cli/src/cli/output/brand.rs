//! The Phoxal wordmark shown once at the top of a startup.
//!
//! The shadowed banner needs 64 columns to stay square, so anything narrower -
//! and every non-interactive stream, where a multi-line block would only be
//! noise in a log - gets the single-line brand instead.

use phoxal_cli_ui::Theme;

const BANNER: &str = "   ██████╗ ██╗  ██╗ ██████╗ ██╗  ██╗ █████╗ ██╗\n   ██╔══██╗██║  ██║██╔═══██╗╚██╗██╔╝██╔══██╗██║\n   ██████╔╝███████║██║   ██║ ╚███╔╝ ███████║██║\n   ██╔═══╝ ██╔══██║██║   ██║ ██╔██╗ ██╔══██║██║\n   ██║     ██║  ██║╚██████╔╝██╔╝ ██╗██║  ██║███████╗\n   ╚═╝     ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝\n             R O B O T I C S   F R A M E W O R K";

/// The minimum width the shadowed wordmark stays square at.
pub(crate) const BANNER_WIDTH: usize = 64;

pub(crate) const FALLBACK: &str = "PHOXAL · ROBOTICS FRAMEWORK";

#[must_use]
pub(crate) fn render(interactive: bool, width: usize, theme: Theme) -> String {
    if interactive && width >= BANNER_WIDTH && theme.supports_unicode() {
        theme.accent(BANNER)
    } else {
        theme.accent(FALLBACK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_ui::ColorCapability;

    #[test]
    fn narrow_or_noninteractive_output_uses_the_single_line_brand() {
        let theme = Theme::new(ColorCapability::None);
        assert_eq!(render(true, 63, theme), FALLBACK);
        assert_eq!(render(false, 200, theme), FALLBACK);
        assert!(render(true, 64, theme).contains("██████"));
        assert!(render(true, 64, theme).contains("R O B O T I C S"));
    }
}
