//! The speed window is measured against the clock, not against the newest
//! sample: a copy that stalls must see its speed decay to zero instead of
//! freezing at the last value it had while bytes were still arriving.

use dirsync::progress::new_progress_channel;
use std::time::{Duration, Instant};

#[test]
fn speed_decays_during_a_stall_and_reaches_zero_after_the_window() {
    let (state, _rx) = new_progress_channel();
    state.reset(100 * 1_048_576, 1);
    for _ in 0..5 {
        state.record_bytes(1_048_576);
        std::thread::sleep(Duration::from_millis(20));
    }
    let now = Instant::now();

    let live = state.speed_mbps_at(now);
    assert!(live > 0.0, "speed while copying should be positive: {live}");

    // A few seconds without new bytes: the same bytes spread over a longer
    // interval, so the reported speed must drop.
    let stalled = state.speed_mbps_at(now + Duration::from_secs(5));
    assert!(
        stalled < live,
        "speed should decay during a stall: {stalled} >= {live}"
    );

    // Once every sample is older than the 10 s window, nothing is moving.
    assert_eq!(state.speed_mbps_at(now + Duration::from_secs(11)), 0.0);
}

#[test]
fn speed_without_samples_is_zero() {
    let (state, _rx) = new_progress_channel();
    assert_eq!(state.speed_mbps_at(Instant::now()), 0.0);
}
