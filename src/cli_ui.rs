use crate::progress::{ProgressEvent, ProgressState, ScanPhase, SyncStatus};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const FILE_BAR_THRESHOLD: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(100);

pub struct CliUi {
    _multi: MultiProgress,
    file_bar: ProgressBar,
    overall_bar: ProgressBar,
}

impl Default for CliUi {
    fn default() -> Self {
        Self::new()
    }
}

impl CliUi {
    pub fn new() -> Self {
        let multi = MultiProgress::new();

        let file_bar = multi.add(ProgressBar::new(100));
        file_bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} [{bar:40.cyan/blue}] {pos:>3}% {msg}")
                .unwrap()
                .progress_chars("=>-"),
        );
        file_bar.set_draw_target(indicatif::ProgressDrawTarget::hidden());

        let overall_bar = multi.add(ProgressBar::new(1000));
        overall_bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{bar:40.green/white}] {percent:>3}% {msg}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );

        Self {
            _multi: multi,
            file_bar,
            overall_bar,
        }
    }

    /// Drive spinners during `engine.preview()`. Returns when the scan finishes
    /// (StatusChanged → Idle/Cancelled) or the channel closes.
    pub async fn scan(mut rx: broadcast::Receiver<ProgressEvent>) {
        let multi = MultiProgress::new();

        let spinner_style = ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);

        let src_bar = multi.add(ProgressBar::new_spinner());
        src_bar.set_style(spinner_style.clone());
        src_bar.set_message("src  Scanning…");
        src_bar.enable_steady_tick(Duration::from_millis(80));

        let dst_bar = multi.add(ProgressBar::new_spinner());
        dst_bar.set_style(spinner_style);
        dst_bar.set_message("dst  Scanning…");
        dst_bar.enable_steady_tick(Duration::from_millis(80));

        loop {
            match rx.recv().await {
                Ok(ProgressEvent::ScanProgress { phase, path }) => match &phase {
                    ScanPhase::Walking { side } => {
                        let (bar, prefix) = if side == "src" {
                            (&src_bar, "src")
                        } else {
                            (&dst_bar, "dst")
                        };
                        let msg = match path.as_deref() {
                            Some("Done.") => format!("{prefix}  Done."),
                            Some(p) if !p.is_empty() => format!("{prefix}  Walking  {p}"),
                            _ => format!("{prefix}  Walking  …"),
                        };
                        bar.set_message(msg);
                        bar.tick();
                    }
                    ScanPhase::Hashing => {
                        let msg = match &path {
                            Some(p) => format!("Fingerprinting  {p}"),
                            None => "Matching…".to_owned(),
                        };
                        src_bar.set_message(msg.clone());
                        src_bar.tick();
                        dst_bar.set_message(msg);
                        dst_bar.tick();
                    }
                    ScanPhase::Planning => {
                        src_bar.set_message("Planning…");
                        src_bar.tick();
                        dst_bar.set_message("Planning…");
                        dst_bar.tick();
                    }
                },
                Ok(ProgressEvent::StatusChanged {
                    status: SyncStatus::Idle | SyncStatus::Cancelled,
                }) => break,
                Err(broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }

        src_bar.finish_and_clear();
        dst_bar.finish_and_clear();
    }

    pub async fn run(
        &self,
        progress: Arc<ProgressState>,
        mut rx: broadcast::Receiver<ProgressEvent>,
    ) {
        // Local shadow state: updated from events, flushed to indicatif on tick only.
        let mut file_started_at: Option<Instant> = None;
        let mut file_visible = false;
        let mut file_size: u64 = 0;
        let mut done: Option<SyncStatus> = None;

        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;

                // Drain events first so we always have the freshest state before drawing.
                result = rx.recv() => {
                    match result {
                        Ok(event) => match event {
                            ProgressEvent::FileStarted { size, .. } => {
                                file_started_at = Some(Instant::now());
                                file_size = size;
                            }
                            ProgressEvent::FileDone { .. } => {
                                file_started_at = None;
                            }
                            ProgressEvent::StatusChanged { status } => {
                                match status {
                                    SyncStatus::Done | SyncStatus::Cancelled => {
                                        done = Some(status);
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        },
                        Err(broadcast::error::RecvError::Closed) => {
                            // Sender dropped without a Done/Cancelled: treat as done.
                            self.overall_bar.abandon_with_message("(disconnected)");
                            self.file_bar.finish_and_clear();
                            return;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {} // skip missed events
                    }
                }

                _ = interval.tick() => {
                    // Decide file bar visibility.
                    let should_show = file_started_at
                        .map(|t| t.elapsed() >= FILE_BAR_THRESHOLD)
                        .unwrap_or(false);

                    if should_show && !file_visible {
                        self.file_bar.set_message(crate::fmt::fmt_bytes_styled(
                            file_size,
                            None,
                            Some(crate::fmt::UNIT_MB),
                            crate::fmt::UNIT_TB,
                        ));
                        self.file_bar.set_draw_target(indicatif::ProgressDrawTarget::stderr());
                        file_visible = true;
                    } else if !should_show && file_visible {
                        self.file_bar.set_draw_target(indicatif::ProgressDrawTarget::hidden());
                        self.file_bar.set_position(0);
                        file_visible = false;
                    }

                    if file_visible {
                        self.file_bar.set_position(progress.file_pct() as u64);
                    }

                    // Overall bar.
                    self.update_overall(&progress);

                    // Terminal condition: flush then return.
                    if let Some(ref status) = done {
                        match status {
                            SyncStatus::Done => {
                                self.file_bar.finish_and_clear();
                                self.overall_bar.finish_with_message("Done");
                            }
                            _ => {
                                self.file_bar.finish_and_clear();
                                self.overall_bar.abandon_with_message("Cancelled");
                            }
                        }
                        return;
                    }
                }
            }
        }
    }

    fn update_overall(&self, progress: &ProgressState) {
        let elapsed = progress.elapsed_secs();
        let speed = progress.speed_mbps();
        let eta = progress.eta_secs();

        let elapsed_str = format_duration(elapsed);
        let remaining_str = eta.map(format_duration).unwrap_or_else(|| "-".into());
        let eta_str = if let Some(secs) = eta {
            use chrono::Local;
            let arrival = Local::now() + chrono::Duration::seconds(secs as i64);
            let now_date = Local::now().date_naive();
            let arr_date = arrival.date_naive();
            if arr_date == now_date {
                format!("ETA {}", arrival.format("%H:%M"))
            } else {
                format!("ETA {}", arrival.format("%a %H:%M"))
            }
        } else {
            "ETA -".into()
        };

        let ops_done = progress.ops_done.load(std::sync::atomic::Ordering::Relaxed);
        let ops_total = progress
            .ops_total
            .load(std::sync::atomic::Ordering::Relaxed);
        let pct_10 = (progress.overall_pct().min(100.0) * 10.0) as u64;

        // Status bar order: Ops | Elapsed | Remaining | ETA | Speed
        self.overall_bar.set_message(format!(
            "Ops: {}/{ops_total_fmt}  Elapsed: {elapsed_str}  Remaining: {remaining_str}  {eta_str}  Speed: {speed:.1} MB/s",
            crate::fmt::fmt_count(ops_done),
            ops_total_fmt = crate::fmt::fmt_count(ops_total),
        ));
        self.overall_bar.set_position(pct_10.min(1000));
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::new_progress_channel;
    use std::sync::atomic::Ordering;

    /// Both loops end on their own once the terminal event arrives; the
    /// timeout only keeps a regression from hanging the suite forever.
    const LIMIT: Duration = Duration::from_secs(10);

    #[test]
    fn format_duration_switches_unit_with_magnitude() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(60), "1m00s");
        assert_eq!(format_duration(3599), "59m59s");
        assert_eq!(format_duration(3600), "1h00m");
        assert_eq!(format_duration(3725), "1h02m");
    }

    #[test]
    fn update_overall_clamps_the_bar_to_its_length() {
        let (progress, _rx) = new_progress_channel();
        progress.ops_total.store(4, Ordering::Relaxed);
        progress.ops_done.store(4, Ordering::Relaxed);
        progress.total_bytes.store(100, Ordering::Relaxed);
        // More bytes than planned: overall_pct must not push the bar past full.
        progress.done_bytes.store(400, Ordering::Relaxed);

        let ui = CliUi::default();
        ui.update_overall(&progress);

        assert_eq!(ui.overall_bar.length(), Some(1000));
        assert!(ui.overall_bar.position() <= 1000);
    }

    #[tokio::test]
    async fn scan_consumes_every_phase_and_stops_when_idle() {
        let (progress, rx) = new_progress_channel();

        for side in ["src", "dst"] {
            progress.emit(ProgressEvent::ScanProgress {
                phase: ScanPhase::Walking {
                    side: side.to_owned(),
                },
                path: Some("some/file".to_owned()),
            });
            progress.emit(ProgressEvent::ScanProgress {
                phase: ScanPhase::Walking {
                    side: side.to_owned(),
                },
                path: Some(String::new()),
            });
            progress.emit(ProgressEvent::ScanProgress {
                phase: ScanPhase::Walking {
                    side: side.to_owned(),
                },
                path: None,
            });
            progress.emit(ProgressEvent::ScanProgress {
                phase: ScanPhase::Walking {
                    side: side.to_owned(),
                },
                path: Some("Done.".to_owned()),
            });
        }
        progress.emit(ProgressEvent::ScanProgress {
            phase: ScanPhase::Hashing,
            path: Some("file".to_owned()),
        });
        progress.emit(ProgressEvent::ScanProgress {
            phase: ScanPhase::Hashing,
            path: None,
        });
        progress.emit(ProgressEvent::ScanProgress {
            phase: ScanPhase::Planning,
            path: None,
        });
        // Ignored by the scan loop, and must not end it.
        progress.emit(ProgressEvent::PlanReady);
        progress.emit(ProgressEvent::StatusChanged {
            status: SyncStatus::Idle,
        });

        tokio::time::timeout(LIMIT, CliUi::scan(rx))
            .await
            .expect("scan should stop on the Idle status");
    }

    #[tokio::test]
    async fn scan_stops_when_the_sender_is_dropped() {
        let (progress, rx) = new_progress_channel();
        drop(progress);

        tokio::time::timeout(LIMIT, CliUi::scan(rx))
            .await
            .expect("scan should stop once the channel closes");
    }

    #[tokio::test]
    async fn scan_stops_on_a_cancelled_status() {
        let (progress, rx) = new_progress_channel();
        progress.emit(ProgressEvent::StatusChanged {
            status: SyncStatus::Cancelled,
        });

        tokio::time::timeout(LIMIT, CliUi::scan(rx))
            .await
            .expect("scan should stop on the Cancelled status");
    }

    #[tokio::test]
    async fn run_tracks_a_file_and_finishes_on_done() {
        let (progress, rx) = new_progress_channel();
        progress.emit(ProgressEvent::FileStarted {
            name: "a.txt".to_owned(),
            size: 4096,
        });
        progress.emit(ProgressEvent::FileProgress { done_bytes: 2048 });
        progress.emit(ProgressEvent::FileDone {
            name: "a.txt".to_owned(),
        });
        // Neither status ends the loop, unlike Done/Cancelled below.
        progress.emit(ProgressEvent::StatusChanged {
            status: SyncStatus::Running,
        });
        progress.emit(ProgressEvent::StatusChanged {
            status: SyncStatus::Done,
        });

        let ui = CliUi::new();
        tokio::time::timeout(LIMIT, ui.run(Arc::clone(&progress), rx))
            .await
            .expect("run should return once the sync is done");
    }

    #[tokio::test]
    async fn run_finishes_on_cancelled() {
        let (progress, rx) = new_progress_channel();
        progress.emit(ProgressEvent::StatusChanged {
            status: SyncStatus::Cancelled,
        });

        let ui = CliUi::new();
        tokio::time::timeout(LIMIT, ui.run(Arc::clone(&progress), rx))
            .await
            .expect("run should return once the sync is cancelled");
    }

    #[tokio::test]
    async fn run_returns_when_the_sender_disappears() {
        let (progress, _rx) = new_progress_channel();
        // A channel of its own, closed immediately: the sync task dying without
        // a final status must not leave the UI spinning forever.
        let (tx, rx) = broadcast::channel(4);
        drop(tx);

        let ui = CliUi::new();
        tokio::time::timeout(LIMIT, ui.run(progress, rx))
            .await
            .expect("run should return when the sender is gone");
    }
}
