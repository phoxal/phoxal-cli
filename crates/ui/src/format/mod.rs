//! Human-readable formatting owned by terminal presentation.

#[must_use]
pub fn duration(value: std::time::Duration) -> String {
    if value < std::time::Duration::from_secs(1) {
        return format!("{}ms", value.as_millis());
    }
    if value < std::time::Duration::from_secs(60) {
        return format!("{:.1}s", value.as_secs_f64());
    }
    let seconds = value.as_secs();
    if seconds < 60 * 60 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!("{}h {:02}m", seconds / (60 * 60), (seconds / 60) % 60)
}
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
