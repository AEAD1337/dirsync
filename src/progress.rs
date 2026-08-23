use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Idle,
    Previewing,
    Running,
    Paused,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanPhase {
    Walking { side: String },
    Hashing,
    Planning,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
    pub run: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressEvent {
    FileStarted {
        name: String,
        size: u64,
    },
    FileProgress {
        done_bytes: u64,
    },
    FileDone {
        name: String,
    },
    FileError {
        name: String,
        message: String,
    },
    OpDone {
        summary: String,
        path: String,
    },
    StatusChanged {
        status: SyncStatus,
    },
    /// Emitted periodically during the walk, and once each for hashing/planning.
    ScanProgress {
        phase: ScanPhase,
        path: Option<String>,
    },
    /// Emitted after each side's directory walk completes during preview.
    ScanUpdate {
        side: String,
        file_count: usize,
    },
    /// Emitted immediately after drive detection at the start of each preview.
    DriveMode {
        hdd: bool,
    },
    /// Emitted when the full preview plan is ready.
    PlanReady,
    /// Emitted just before the server shuts down so clients can close their tab.
    Shutdown,
    /// Emitted by any subsystem to add a line to the GUI log panel.
    LogEntry(LogEntry),
}

pub struct ProgressState {
    pub status: RwLock<SyncStatus>,
    pub total_bytes: AtomicU64,
    pub done_bytes: AtomicU64,
    pub ops_total: AtomicUsize,
    pub ops_done: AtomicUsize,
    pub current_file: RwLock<Option<String>>,
    pub current_file_size: AtomicU64,
    pub current_file_done: AtomicU64,
    pub started_at: Mutex<Option<Instant>>,
    /// When the timer was last paused (None = not currently paused).
    paused_at: Mutex<Option<Instant>>,
    /// Set when the run finishes; freezes `elapsed_secs` at the final total
    /// instead of resetting the display to zero.
    stopped_at: Mutex<Option<Instant>>,
    /// Total time already spent in paused state before the current pause.
    total_paused: Mutex<Duration>,
    /// Ring buffer: (instant, cumulative_bytes_at_that_point)
    speed_ring: Mutex<VecDeque<(Instant, u64)>>,
    tx: broadcast::Sender<ProgressEvent>,
    pub run: AtomicU32,
}

impl ProgressState {
    pub fn new(tx: broadcast::Sender<ProgressEvent>) -> Self {
        Self {
            status: RwLock::new(SyncStatus::Idle),
            total_bytes: AtomicU64::new(0),
            done_bytes: AtomicU64::new(0),
            ops_total: AtomicUsize::new(0),
            ops_done: AtomicUsize::new(0),
            current_file: RwLock::new(None),
            current_file_size: AtomicU64::new(0),
            current_file_done: AtomicU64::new(0),
            started_at: Mutex::new(None),
            paused_at: Mutex::new(None),
            stopped_at: Mutex::new(None),
            total_paused: Mutex::new(Duration::ZERO),
            speed_ring: Mutex::new(VecDeque::new()),
            tx,
            run: AtomicU32::new(0),
        }
    }

    pub fn reset(&self, total_bytes: u64, ops_total: usize) {
        self.total_bytes.store(total_bytes, Ordering::Relaxed);
        self.done_bytes.store(0, Ordering::Relaxed);
        self.ops_total.store(ops_total, Ordering::Relaxed);
        self.ops_done.store(0, Ordering::Relaxed);
        self.current_file_done.store(0, Ordering::Relaxed);
        self.current_file_size.store(0, Ordering::Relaxed);
        *self.current_file.write().unwrap() = None;
        *self.started_at.lock().unwrap() = Some(Instant::now());
        *self.paused_at.lock().unwrap() = None;
        *self.stopped_at.lock().unwrap() = None;
        *self.total_paused.lock().unwrap() = Duration::ZERO;
        self.speed_ring.lock().unwrap().clear();
    }

    pub fn emit(&self, event: ProgressEvent) {
        let _ = self.tx.send(event);
    }

    /// Increment the run counter (call once per preview start) and return the new value.
    pub fn next_run(&self) -> u32 {
        self.run.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn current_run(&self) -> u32 {
        self.run.load(Ordering::Relaxed)
    }

    /// Convenience: build a LogEntry from the current run and emit it.
    pub fn emit_log(&self, level: LogLevel, message: String) {
        let _ = self.tx.send(ProgressEvent::LogEntry(LogEntry {
            level,
            message,
            run: self.run.load(Ordering::Relaxed),
        }));
    }

    pub fn record_bytes(&self, bytes: u64) {
        let now_done = self.done_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let mut ring = self.speed_ring.lock().unwrap();
        ring.push_back((Instant::now(), now_done));
        // Keep only last 10 seconds
        // checked_sub: `Instant - Duration` panics on underflow, which is
        // reachable on clocks whose epoch is process/boot start.
        let Some(cutoff) = Instant::now().checked_sub(Duration::from_secs(10)) else {
            return;
        };
        while ring.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            ring.pop_front();
        }
    }

    /// MB/s average over last 10 s window.
    pub fn speed_mbps(&self) -> f64 {
        let ring = self.speed_ring.lock().unwrap();
        if ring.len() < 2 {
            return 0.0;
        }
        let (t0, b0) = ring.front().unwrap();
        let (t1, b1) = ring.back().unwrap();
        let elapsed = t1.duration_since(*t0).as_secs_f64();
        if elapsed < 0.001 {
            return 0.0;
        }
        (b1.saturating_sub(*b0)) as f64 / elapsed / 1_048_576.0
    }

    pub fn elapsed_secs(&self) -> u64 {
        let started = *self.started_at.lock().unwrap();
        let Some(t) = started else { return 0 };
        // Once stopped, measure to the stop instant so the final duration
        // stays on screen rather than ticking on or snapping back to zero.
        let total = match *self.stopped_at.lock().unwrap() {
            Some(end) => end.saturating_duration_since(t),
            None => t.elapsed(),
        };
        let paused = *self.total_paused.lock().unwrap();
        let current_pause = self
            .paused_at
            .lock()
            .unwrap()
            .map(|p| p.elapsed())
            .unwrap_or(Duration::ZERO);
        total.saturating_sub(paused + current_pause).as_secs()
    }

    /// Freeze the elapsed timer (called when execution pauses).
    pub fn pause_timer(&self) {
        *self.paused_at.lock().unwrap() = Some(Instant::now());
    }

    /// Resume the elapsed timer (called when execution resumes).
    pub fn resume_timer(&self) {
        let mut paused_at = self.paused_at.lock().unwrap();
        if let Some(t) = paused_at.take() {
            *self.total_paused.lock().unwrap() += t.elapsed();
        }
    }

    /// Freeze the elapsed timer (called when sync finishes or is cancelled).
    /// The total stays readable until the next `reset`.
    pub fn stop_timer(&self) {
        let mut stopped = self.stopped_at.lock().unwrap();
        if stopped.is_none() {
            *stopped = Some(Instant::now());
        }
        // Fold any in-progress pause in so it is excluded from the final total.
        if let Some(p) = self.paused_at.lock().unwrap().take() {
            *self.total_paused.lock().unwrap() += p.elapsed();
        }
    }

    pub fn eta_secs(&self) -> Option<u64> {
        let speed = self.speed_mbps();
        if speed < 0.001 {
            return None;
        }
        let remaining = self
            .total_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(self.done_bytes.load(Ordering::Relaxed));
        Some((remaining as f64 / (speed * 1_048_576.0)) as u64)
    }

    pub fn overall_pct(&self) -> f64 {
        let total = self.total_bytes.load(Ordering::Relaxed);
        if total == 0 {
            return 100.0;
        }
        let pct = self.done_bytes.load(Ordering::Relaxed) as f64 / total as f64 * 100.0;
        pct.min(100.0)
    }

    pub fn file_pct(&self) -> f32 {
        let total = self.current_file_size.load(Ordering::Relaxed);
        if total == 0 {
            return 100.0;
        }
        let done = self.current_file_done.load(Ordering::Relaxed);
        done as f32 / total as f32 * 100.0
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProgressEvent> {
        self.tx.subscribe()
    }
}

pub fn new_progress_channel() -> (Arc<ProgressState>, broadcast::Receiver<ProgressEvent>) {
    let (tx, rx) = broadcast::channel(1024);
    (Arc::new(ProgressState::new(tx)), rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_state() -> Arc<ProgressState> {
        let (tx, _rx) = broadcast::channel(16);
        Arc::new(ProgressState::new(tx))
    }

    #[test]
    fn test_elapsed_secs_before_reset_is_zero() {
        let s = make_state();
        assert_eq!(s.elapsed_secs(), 0);
    }

    #[test]
    fn test_elapsed_secs_after_stop_is_zero() {
        let s = make_state();
        s.reset(0, 0);
        std::thread::sleep(Duration::from_millis(20));
        s.stop_timer();
        assert_eq!(s.elapsed_secs(), 0);
    }

    #[test]
    fn test_pause_timer_excludes_paused_time_from_elapsed() {
        let s = make_state();
        s.reset(0, 0);

        // Pause immediately, sleep 600 ms, resume.
        // The paused interval must not count toward elapsed.
        s.pause_timer();
        std::thread::sleep(Duration::from_millis(600));
        s.resume_timer();

        // Total elapsed should be < 600 ms → 0 whole seconds.
        assert_eq!(s.elapsed_secs(), 0, "paused time should not count");
    }

    #[test]
    fn test_speed_mbps_zero_before_any_data() {
        let s = make_state();
        s.reset(10 * 1024 * 1024, 1);
        assert_eq!(s.speed_mbps(), 0.0);
    }

    #[test]
    fn test_speed_mbps_positive_after_recording_bytes() {
        let s = make_state();
        s.reset(100 * 1024 * 1024, 1);

        s.record_bytes(5 * 1024 * 1024);
        std::thread::sleep(Duration::from_millis(50));
        s.record_bytes(5 * 1024 * 1024);

        assert!(
            s.speed_mbps() > 0.0,
            "speed should be positive after recording bytes"
        );
    }

    #[test]
    fn test_overall_pct_is_100_when_total_is_zero() {
        let s = make_state();
        s.reset(0, 0);
        assert_eq!(s.overall_pct(), 100.0);
    }

    #[test]
    fn test_overall_pct_tracks_done_bytes() {
        let s = make_state();
        s.reset(1000, 1);
        s.record_bytes(500);
        let pct = s.overall_pct();
        assert!((pct - 50.0).abs() < 1.0, "expected ~50%, got {pct}");
    }

    #[test]
    fn test_overall_pct_clamps_to_100() {
        let s = make_state();
        s.reset(100, 1);
        s.record_bytes(200); // more than total
        assert_eq!(s.overall_pct(), 100.0);
    }

    #[test]
    fn test_eta_secs_none_before_speed_established() {
        let s = make_state();
        s.reset(10 * 1024 * 1024, 1);
        assert!(s.eta_secs().is_none());
    }

    #[test]
    fn test_eta_secs_some_when_speed_known() {
        let s = make_state();
        s.reset(100 * 1024 * 1024, 1);

        s.record_bytes(10 * 1024 * 1024);
        std::thread::sleep(Duration::from_millis(50));
        s.record_bytes(10 * 1024 * 1024);

        assert!(
            s.eta_secs().is_some(),
            "ETA should be available once speed > 0"
        );
    }

    #[test]
    fn test_file_pct_is_100_when_size_is_zero() {
        let s = make_state();
        s.reset(0, 0);
        assert_eq!(s.file_pct(), 100.0);
    }

    #[test]
    fn test_file_pct_tracks_current_file_progress() {
        let s = make_state();
        s.current_file_size
            .store(200, std::sync::atomic::Ordering::Relaxed);
        s.current_file_done
            .store(100, std::sync::atomic::Ordering::Relaxed);
        let pct = s.file_pct();
        assert!((pct - 50.0).abs() < 1.0, "expected ~50%, got {pct}");
    }

    #[test]
    fn test_log_entry_serializes_with_snake_case_level() {
        let entry = LogEntry {
            level: LogLevel::Warning,
            message: "test message".into(),
            run: 3,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"level\":\"warning\""), "got: {json}");
        assert!(json.contains("\"run\":3"), "got: {json}");
    }

    #[test]
    fn test_emit_log_uses_current_run() {
        let s = make_state();
        s.next_run(); // run = 1
        let mut rx = s.subscribe();
        s.emit_log(LogLevel::Info, "hello".into());
        match rx.try_recv() {
            Ok(ProgressEvent::LogEntry(e)) => {
                assert_eq!(e.run, 1);
                assert_eq!(e.message, "hello");
            }
            other => panic!("expected LogEntry, got {other:?}"),
        }
    }
}
