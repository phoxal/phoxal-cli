//! Human-readable formatting owned by terminal presentation.

pub use phoxal_cli_core::runtime::format_duration as duration;
pub use phoxal_cli_observation::sanitize_terminal_text;

/// Format a byte count using IEC units, keeping exact bytes below one KiB.
#[must_use]
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

/// Compact byte formatting for constrained status-line slots.
#[must_use]
pub fn bytes_compact(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value}B")
    } else {
        format!("{amount:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_across_supported_units() {
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(7_461_785), "7.1 MiB");
        assert_eq!(bytes(3 * 1024_u64.pow(4)), "3.0 TiB");
        assert_eq!(bytes_compact(16 * 1024_u64.pow(3)), "16.0G");
    }
}
