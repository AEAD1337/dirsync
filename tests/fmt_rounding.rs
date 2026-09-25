//! Byte formatting must round at the displayed precision before it picks the
//! unit: otherwise a value just under a boundary renders as the boundary in
//! the smaller unit ("1024 KB", "10.00 KB") instead of promoting.

use dirsync::fmt::{UNIT_GB, UNIT_MB, UNIT_TB, fmt_bytes, fmt_bytes_styled};

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;

#[test]
fn auto_scale_promotes_when_rounding_reaches_1024() {
    // 1,048,575 B is 1023.999 KB: "{:.0}" would print 1024 KB.
    assert_eq!(fmt_bytes(MB - 1), "1.00 MB");
    // 1023.5 KB rounds up to 1024 as well.
    assert_eq!(fmt_bytes(1023 * KB + 512), "1.00 MB");
    // Just below the rounding point stays in KB.
    assert_eq!(fmt_bytes(1023 * KB + 511), "1023 KB");
}

#[test]
fn auto_scale_picks_precision_after_rounding() {
    // 10235 B = 9.995 KB: two decimals would print "10.00".
    assert_eq!(fmt_bytes(10235), "10.0 KB");
    // 102360 B = 99.96 KB: one decimal would print "100.0".
    assert_eq!(fmt_bytes(102_360), "100 KB");
    // Unchanged below the thresholds.
    assert_eq!(fmt_bytes(10234), "9.99 KB");
    assert_eq!(fmt_bytes(102_348), "99.9 KB");
}

#[test]
fn fixed_decimals_promote_when_rounding_reaches_1024() {
    let gui = |b| fmt_bytes_styled(b, Some(1), None, UNIT_GB);
    assert_eq!(gui(MB - 1), "1.0 MB");
    assert_eq!(gui(1024 * MB - 1), "1.0 GB");
    assert_eq!(gui(1023 * KB), "1023.0 KB");
    // Whole bytes are exact, so they never promote early.
    assert_eq!(gui(1023), "1023 B");
}

#[test]
fn the_unit_cap_never_promotes_past_the_last_unit() {
    let gui = |b| fmt_bytes_styled(b, Some(1), None, UNIT_GB);
    assert_eq!(gui(2048 * 1024 * MB), "2048.0 GB");
    assert_eq!(fmt_bytes(2048 * 1024 * 1024 * MB), "2048.0 TB");
}

#[test]
fn a_fixed_unit_is_never_promoted() {
    let mb = |b| fmt_bytes_styled(b, None, Some(UNIT_MB), UNIT_TB);
    assert_eq!(mb(1024 * MB - 1), "1024.0 MB");
}
