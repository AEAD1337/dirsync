/// Unit indexes for [`fmt_bytes_styled`]'s `fixed_unit` / `max_unit`.
pub const UNIT_MB: usize = 2;
pub const UNIT_GB: usize = 3;
pub const UNIT_TB: usize = 4;

const BYTE_UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];

/// Human-readable byte count: the one implementation behind every
/// byte-rendering site. The parameters preserve each site's established
/// style rather than imposing one:
///
/// * `decimals`: `None`: precision by magnitude (2 dp below 10, 1 dp below
///   100, 0 dp above). `Some(d)`: exactly `d` decimals for scaled units,
///   whole bytes printed as a plain integer.
/// * `fixed_unit`: `Some(unit)`: always render in that unit (the per-file
///   "12.3 MB" log lines). `None`: auto-scale.
/// * `max_unit`: cap for auto-scaling (the GUI log keeps its GB cap).
pub fn fmt_bytes_styled(
    bytes: u64,
    decimals: Option<usize>,
    fixed_unit: Option<usize>,
    max_unit: usize,
) -> String {
    let max_unit = max_unit.min(BYTE_UNITS.len() - 1);
    if let Some(idx) = fixed_unit {
        let idx = idx.min(BYTE_UNITS.len() - 1);
        let val = bytes as f64 / 1024f64.powi(idx as i32);
        let d = decimals.unwrap_or(1);
        return format!("{val:.d$} {}", BYTE_UNITS[idx]);
    }
    let mut val = bytes as f64;
    for (i, &unit) in BYTE_UNITS.iter().enumerate().take(max_unit) {
        if val < 1024.0 {
            return match decimals {
                Some(_) if i == 0 => format!("{bytes} B"),
                Some(d) => format!("{val:.d$} {unit}"),
                None if val < 10.0 => format!("{val:.2} {unit}"),
                None if val < 100.0 => format!("{val:.1} {unit}"),
                None => format!("{val:.0} {unit}"),
            };
        }
        val /= 1024.0;
    }
    let d = decimals.unwrap_or(1);
    format!("{val:.d$} {}", BYTE_UNITS[max_unit])
}

/// Auto-scaled B→TB with precision by magnitude: the CLI plan-summary style.
pub fn fmt_bytes(bytes: u64) -> String {
    fmt_bytes_styled(bytes, None, None, UNIT_TB)
}

/// Format an integer with thousands separators: 3819342 → "3,819,342".
pub fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_count_small() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
    }

    #[test]
    fn test_fmt_count_large() {
        assert_eq!(fmt_count(3_819_342), "3,819,342");
        assert_eq!(fmt_count(1_000_000_000), "1,000,000,000");
    }

    // --- fmt_bytes (adaptive precision, B→TB) ---

    #[test]
    fn test_fmt_bytes_b_scale() {
        assert_eq!(fmt_bytes(0), "0.00 B");
        assert_eq!(fmt_bytes(9), "9.00 B");
        assert_eq!(fmt_bytes(10), "10.0 B");
        assert_eq!(fmt_bytes(99), "99.0 B");
        assert_eq!(fmt_bytes(100), "100 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
    }

    #[test]
    fn test_fmt_bytes_kb_to_gb() {
        assert_eq!(fmt_bytes(1024), "1.00 KB");
        assert_eq!(fmt_bytes(10 * 1024), "10.0 KB");
        assert_eq!(fmt_bytes(100 * 1024), "100 KB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(fmt_bytes(10 * 1024 * 1024), "10.0 MB");
        assert_eq!(fmt_bytes(100 * 1024 * 1024), "100 MB");
        assert_eq!(fmt_bytes(1024u64 * 1024 * 1024), "1.00 GB");
        assert_eq!(fmt_bytes(10 * 1024u64 * 1024 * 1024), "10.0 GB");
        assert_eq!(fmt_bytes(100 * 1024u64 * 1024 * 1024), "100 GB");
    }

    #[test]
    fn test_fmt_bytes_tb() {
        assert_eq!(fmt_bytes(1024u64 * 1024 * 1024 * 1024), "1.0 TB");
        assert_eq!(fmt_bytes(10 * 1024u64 * 1024 * 1024 * 1024), "10.0 TB");
    }

    // --- fmt_bytes_styled: GUI log style (fixed 1 dp, GB cap) ---

    #[test]
    fn test_fmt_bytes_styled_gui_log() {
        let gui = |b| fmt_bytes_styled(b, Some(1), None, UNIT_GB);
        assert_eq!(gui(500), "500 B");
        assert_eq!(gui(1500), "1.5 KB");
        assert_eq!(gui(1_500_000), "1.4 MB");
        assert_eq!(gui(1_500_000_000), "1.4 GB");
        // The GB cap is preserved: no TB unit in the GUI log.
        assert_eq!(gui(2 * 1024u64 * 1024 * 1024 * 1024), "2048.0 GB");
    }

    // --- fmt_bytes_styled: per-file log style (forced MB) ---

    #[test]
    fn test_fmt_bytes_styled_forced_mb() {
        let mb = |b| fmt_bytes_styled(b, None, Some(UNIT_MB), UNIT_TB);
        assert_eq!(mb(0), "0.0 MB");
        assert_eq!(mb(1_048_576), "1.0 MB");
        assert_eq!(mb(15 * 1024 * 1024 * 1024), "15360.0 MB");
    }
}
