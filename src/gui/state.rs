use crate::config::AppConfig;
use crate::progress::{LogEntry, ProgressState, new_progress_channel};
use crate::sync::planner::SyncPlan;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::watch;

pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub progress: Arc<ProgressState>,
    pub last_plan: RwLock<Option<SyncPlan>>,
    pub pause_tx: watch::Sender<bool>,
    pub cancel_tx: watch::Sender<bool>,
    /// Set to `true` to trigger graceful server shutdown.
    pub shutdown_tx: watch::Sender<bool>,
    /// When `true` the frontend auto-triggers a preview on first load.
    pub auto_preview: bool,
    /// When `true` system-critical path checks are skipped.
    pub yolo: bool,
    /// Ring buffer of log entries; capped at 2000. Written by a single
    /// server-side task so the contents do not depend on how many browser
    /// tabs happen to be open.
    pub log_buffer: Mutex<VecDeque<LogEntry>>,
    /// Live WebSocket connections. When this reaches zero the server waits a
    /// short grace period and then shuts down: a page reload reconnects well
    /// inside it, so refreshing no longer kills the backend.
    pub ws_clients: AtomicUsize,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        auto_preview: bool,
        yolo: bool,
    ) -> (
        Arc<Self>,
        tokio::sync::broadcast::Receiver<crate::progress::ProgressEvent>,
    ) {
        let (progress, rx) = new_progress_channel();
        let (pause_tx, _) = watch::channel(false);
        let (cancel_tx, _) = watch::channel(false);
        let (shutdown_tx, _) = watch::channel(false);

        let state = Arc::new(Self {
            config: Arc::new(RwLock::new(config)),
            progress,
            last_plan: RwLock::new(None),
            pause_tx,
            cancel_tx,
            shutdown_tx,
            auto_preview,
            yolo,
            log_buffer: Mutex::new(VecDeque::new()),
            ws_clients: AtomicUsize::new(0),
        });

        (state, rx)
    }

    pub fn pause_rx(&self) -> watch::Receiver<bool> {
        self.pause_tx.subscribe()
    }

    pub fn cancel_rx(&self) -> watch::Receiver<bool> {
        self.cancel_tx.subscribe()
    }

    /// Clear pause/cancel before handing the channels to a new preview or run.
    ///
    /// `send_replace` rather than `send`: `send` is a no-op when no receiver is
    /// alive, and between a finished run and the next preview there is none:
    /// so a cancelled run left `cancel` latched at `true` and the next preview
    /// cancelled itself immediately. See the test below.
    pub fn reset_control(&self) {
        self.pause_tx.send_replace(false);
        self.cancel_tx.send_replace(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `watch::Sender::send` is a no-op when every receiver has been dropped:
    /// and between a finished run and the next preview there are none, since
    /// only the executor/preview task holds one. Clearing the flags with
    /// `send` therefore left `cancel` stuck at `true` after a cancelled run,
    /// and the next preview aborted the moment it looked at the token.
    #[test]
    fn reset_control_clears_flags_after_the_last_receiver_is_gone() {
        let (state, _events) = AppState::new(AppConfig::default(), false, false);

        // A run is in flight: it holds the only receivers, and gets cancelled.
        {
            let cancel_rx = state.cancel_rx();
            let pause_rx = state.pause_rx();
            state.cancel_tx.send_replace(true);
            state.pause_tx.send_replace(true);
            assert!(*cancel_rx.borrow());
            assert!(*pause_rx.borrow());
        } // run ends: receivers dropped

        state.reset_control();

        assert!(
            !*state.cancel_rx().borrow(),
            "cancel flag survived reset_control - next preview cancels itself"
        );
        assert!(
            !*state.pause_rx().borrow(),
            "pause flag survived reset_control - next run starts paused"
        );
    }
}
