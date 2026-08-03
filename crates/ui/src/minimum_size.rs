//! The supported minimum terminal size, and what to draw below it.
//!
//! The shell splits the frame into fixed header/tabs/footer bands plus a body.
//! Below a certain size those bands consume everything and the layout stops
//! being a layout: panels collapse to their borders and content is silently
//! truncated. That looked like a broken program rather than a small window
//! (organization#974).
//!
//! So below the minimum the shell draws one honest sentence instead. The size
//! arithmetic lives here, apart from any rendering, because it is the part
//! worth testing.

use tuirealm::ratatui::layout::Rect;

/// The smallest terminal the layout is designed to work in. Nominal design
/// size is 100x30; this is the floor below which the shell refuses to paint a
/// layout it cannot honour (organization#974).
pub const MINIMUM_COLUMNS: u16 = 80;
pub const MINIMUM_ROWS: u16 = 24;

/// Whether `area` can hold the shell's layout.
#[must_use]
pub fn fits(area: Rect) -> bool {
    area.width >= MINIMUM_COLUMNS && area.height >= MINIMUM_ROWS
}

/// What to tell someone whose terminal is too small.
///
/// It names both sizes: "too small" alone leaves the reader to guess how much
/// to drag, and the required size alone leaves them unsure whether the program
/// even noticed their window.
#[must_use]
pub fn message(area: Rect) -> String {
    format!(
        "terminal too small ({}x{}, need {MINIMUM_COLUMNS}x{MINIMUM_ROWS})",
        area.width, area.height
    )
}

/// Centre `text` in `area`, clamped so a terminal narrower or shorter than the
/// message still shows the message from its first character rather than
/// scrolling it out of view.
#[must_use]
pub fn centered(area: Rect, text: &str) -> Rect {
    let width = u16::try_from(text.chars().count())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let height = 1.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn the_supported_minimum_itself_fits() {
        assert!(fits(area(MINIMUM_COLUMNS, MINIMUM_ROWS)));
        assert!(fits(area(200, 50)));
    }

    #[test]
    fn one_column_or_row_short_does_not_fit() {
        // Off-by-one here is the difference between a working layout and a
        // truncated one, so both edges are pinned.
        assert!(!fits(area(MINIMUM_COLUMNS - 1, MINIMUM_ROWS)));
        assert!(!fits(area(MINIMUM_COLUMNS, MINIMUM_ROWS - 1)));
    }

    #[test]
    fn the_message_names_the_actual_size_and_the_required_one() {
        assert_eq!(
            message(area(40, 12)),
            "terminal too small (40x12, need 80x24)"
        );
    }

    #[test]
    fn the_message_is_centered() {
        let text = message(area(40, 12));
        let placed = centered(area(40, 12), &text);
        assert_eq!(placed.height, 1);
        assert_eq!(placed.width, 38, "the message is 38 columns wide");
        assert_eq!(placed.x, 1);
        assert_eq!(placed.y, 5);
    }

    #[test]
    fn a_terminal_narrower_than_the_message_still_shows_its_start() {
        // Centring a 38-column message in 10 columns must not push x negative
        // or off-screen; the reader sees the beginning, which carries the
        // words "terminal too small".
        let placed = centered(area(10, 3), &message(area(10, 3)));
        assert_eq!(placed.x, 0);
        assert_eq!(placed.width, 10);
        assert!(placed.y < 3);
    }

    #[test]
    fn a_zero_height_area_yields_nothing_to_draw() {
        let placed = centered(area(80, 0), "anything");
        assert_eq!(placed.height, 0);
    }
}
