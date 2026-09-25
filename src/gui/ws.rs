use crate::gui::handlers::{PlanSummary, plan_to_summary};
use crate::gui::state::AppState;
use crate::progress::{LogLevel, ProgressEvent, ScanPhase, SyncStatus};
use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;
use tokio::time::interval;

pub(crate) const LOG_BUFFER_CAP: usize = 2000;

pub(crate) fn push_log_entry(
    buf: &mut std::collections::VecDeque<crate::progress::LogEntry>,
    entry: crate::progress::LogEntry,
) {
    if buf.len() >= LOG_BUFFER_CAP {
        buf.pop_front();
    }
    buf.push_back(entry);
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    ProgressUpdate {
        done_bytes: u64,
        total_bytes: u64,
        current_file: Option<String>,
        current_file_done: u64,
        current_file_size: u64,
        current_file_pct: f32,
        /// Directory the current chunked copy writes into, relative to
        /// `dst_root`. The client keeps that row's active marker lit while a
        /// long file runs, when no completions are arriving to imply it.
        current_dir: Option<String>,
        speed_mbps: f64,
        elapsed_secs: u64,
        eta_secs: Option<u64>,
        ops_done: usize,
        ops_total: usize,
        status: SyncStatus,
    },
    /// Every status transition, pushed as it happens. The 100 ms tick above
    /// only *samples* the status, so a state the engine passes through inside
    /// one tick window is invisible to the client: and the frontend needs to
    /// see the `previewing` → `idle` edge to know its preview ended (a
    /// cancelled preview produces no `plan_ready` and no `error_occurred`).
    StatusChanged {
        status: SyncStatus,
    },
    /// One op failed. `path` is DST-relative and forward-slashed: the same
    /// form as `OpEntry::rel_path`, so the client can mark that row.
    ErrorOccurred {
        path: String,
        message: String,
    },
    /// The preview failed (not cancelled). Its own event rather than an
    /// `ErrorOccurred` with a sentinel path, which a root-level file of the
    /// same name would collide with.
    PreviewFailed {
        message: String,
    },
    /// Batch of op-completed paths flushed once per tick (~100 ms) instead of
    /// one message per file, to avoid flooding the browser's JS event loop.
    OpsCompleted {
        rel_paths: Vec<String>,
    },
    Shutdown,
    ScanUpdate {
        side: String,
        file_count: usize,
    },
    ScanProgress {
        phase: String,
        path: Option<String>,
    },
    DriveMode {
        hdd: bool,
    },
    /// Sent when preview finishes. Carries the full plan so the frontend can
    /// populate the op tables without a second round-trip.
    PlanReady(PlanSummary),
    LogEntry {
        level: LogLevel,
        message: String,
        run: u32,
    },
}

/// What one progress event means for a WebSocket client.
pub enum WsOutput {
    /// Send this event now.
    Send(WsEvent),
    /// A finished op's DST-relative path, batched into the next tick's
    /// `OpsCompleted` instead of one frame per file.
    Completed(String),
    /// Nothing for the client.
    Skip,
}

/// `path` (absolute, under the current plan's DST root) in the client's row
/// form: DST-relative and forward-slashed.
fn rel_to_plan_dst(state: &AppState, path: String) -> String {
    let dst_root = state
        .last_plan
        .read()
        .unwrap()
        .as_ref()
        .map(|p| p.dst_root.clone());
    match dst_root {
        Some(root) => crate::paths::rel_to_root(std::path::Path::new(&path), &root),
        None => path,
    }
}

/// Translate one engine progress event into what the client receives.
pub fn ws_event_for(state: &AppState, event: ProgressEvent) -> WsOutput {
    let ev = match event {
        ProgressEvent::StatusChanged { status } => WsEvent::StatusChanged { status },
        ProgressEvent::FileError { name, message } => WsEvent::ErrorOccurred {
            path: rel_to_plan_dst(state, name),
            message,
        },
        ProgressEvent::PreviewFailed { message } => WsEvent::PreviewFailed { message },
        ProgressEvent::OpDone { path, .. } => {
            return WsOutput::Completed(rel_to_plan_dst(state, path));
        }
        ProgressEvent::ScanUpdate { side, file_count } => WsEvent::ScanUpdate { side, file_count },
        ProgressEvent::ScanProgress { phase, path } => {
            let phase = match &phase {
                ScanPhase::Walking { side } => format!("walking_{side}"),
                ScanPhase::Hashing => "hashing".to_owned(),
                ScanPhase::Planning => "planning".to_owned(),
            };
            WsEvent::ScanProgress { phase, path }
        }
        ProgressEvent::DriveMode { hdd } => WsEvent::DriveMode { hdd },
        ProgressEvent::PlanReady => {
            // Read the plan and send it in full so the frontend can populate
            // the tables without a separate HTTP call.
            let guard = state.last_plan.read().unwrap();
            match guard.as_ref() {
                Some(plan) => WsEvent::PlanReady(plan_to_summary(plan)),
                None => return WsOutput::Skip,
            }
        }
        ProgressEvent::Shutdown => WsEvent::Shutdown,
        // The ring buffer is filled by a single server-side task (see
        // gui::server::start), not here: writing it per connection
        // duplicated every entry once per open tab and buffered nothing at
        // all with no tab open.
        ProgressEvent::LogEntry(entry) => WsEvent::LogEntry {
            level: entry.level,
            message: entry.message,
            run: entry.run,
        },
        ProgressEvent::FileStarted { .. }
        | ProgressEvent::FileProgress { .. }
        | ProgressEvent::FileDone { .. } => return WsOutput::Skip,
    };
    WsOutput::Send(ev)
}

/// The log line a consumer records when the broadcast channel overran it,
/// so a burst leaves a visible gap marker instead of a silent hole.
pub(crate) fn lag_notice(missed: u64, run: u32) -> crate::progress::LogEntry {
    crate::progress::LogEntry {
        level: LogLevel::Warning,
        message: format!(
            "{missed} progress event(s) dropped under load; the full error list is in the run summary."
        ),
        run,
    }
}

/// How long the server stays alive after the last WebSocket client goes away.
///
/// The frontend used to fire `navigator.sendBeacon('/api/v1/shutdown')` from
/// `beforeunload`, which fires on reload as well as on close: so pressing F5
/// killed the backend and the reloaded page had nothing to connect to. Waiting
/// for a reconnect instead distinguishes the two: a reload is back in well
/// under a second, a closed tab never returns.
const CLIENT_GRACE: Duration = Duration::from_secs(5);

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let rx = state.progress.subscribe();
    ws.on_upgrade(move |socket| async move {
        state.ws_clients.fetch_add(1, AtomicOrdering::SeqCst);
        handle_socket(socket, state.clone(), rx).await;
        // fetch_sub returns the previous value: 1 means we were the last one.
        if state.ws_clients.fetch_sub(1, AtomicOrdering::SeqCst) == 1 {
            tokio::spawn(shutdown_if_no_client_returns(state));
        }
    })
}

async fn shutdown_if_no_client_returns(state: Arc<AppState>) {
    tokio::time::sleep(CLIENT_GRACE).await;
    if state.ws_clients.load(AtomicOrdering::SeqCst) > 0 {
        return; // a reload reconnected
    }
    state.request_shutdown().await;
}

/// The directory the chunked copy path is currently writing into, relative to
/// `dst_root` and forward-slashed.
///
/// `None` when nothing is copying, when the file sits directly in `dst_root`
/// (no directory row exists for it), or when the target is somehow outside
/// `dst_root`: the client matches this against row paths, so an absolute path
/// would be meaningless there as well as a needless disclosure.
pub fn current_dir_rel(state: &AppState) -> Option<String> {
    let dst = state.progress.current_file_dst.read().unwrap().clone()?;
    let parent = dst.parent()?;
    let dst_root = state
        .last_plan
        .read()
        .unwrap()
        .as_ref()
        .map(|p| p.dst_root.clone())?;
    let rel = crate::paths::to_slash(parent.strip_prefix(&dst_root).ok()?);
    if rel.is_empty() { None } else { Some(rel) }
}

async fn handle_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
    mut rx: tokio::sync::broadcast::Receiver<ProgressEvent>,
) {
    let mut tick = interval(Duration::from_millis(100));
    // Buffer completed rel-paths; flushed once per tick as a single message
    // instead of one WS frame per file to avoid flooding the browser's JS loop.
    let mut pending_completed: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let p = &state.progress;
                let status = p.status.read().unwrap().clone();
                let event = WsEvent::ProgressUpdate {
                    done_bytes: p.done_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    total_bytes: p.total_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    current_file: p.current_file.read().unwrap().clone(),
                    current_file_done: p.current_file_done.load(std::sync::atomic::Ordering::Relaxed),
                    current_file_size: p.current_file_size.load(std::sync::atomic::Ordering::Relaxed),
                    current_file_pct: p.file_pct(),
                    current_dir: current_dir_rel(&state),
                    speed_mbps: p.speed_mbps(),
                    elapsed_secs: p.elapsed_secs(),
                    eta_secs: p.eta_secs(),
                    ops_done: p.ops_done.load(std::sync::atomic::Ordering::Relaxed),
                    ops_total: p.ops_total.load(std::sync::atomic::Ordering::Relaxed),
                    status,
                };
                let json = serde_json::to_string(&event).unwrap_or_default();
                if socket.send(Message::Text(json.into())).await.is_err() {
                    return;
                }
                if !pending_completed.is_empty() {
                    let batch = WsEvent::OpsCompleted {
                        rel_paths: std::mem::take(&mut pending_completed),
                    };
                    let json = serde_json::to_string(&batch).unwrap_or_default();
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        return;
                    }
                }
            }

            event = rx.recv() => {
                let out = match event {
                    Ok(event) => ws_event_for(&state, event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        let run = state.progress.current_run();
                        let e = lag_notice(missed, run);
                        WsOutput::Send(WsEvent::LogEntry { level: e.level, message: e.message, run })
                    }
                    Err(_) => return,
                };
                match out {
                    WsOutput::Send(ev) => {
                        let is_shutdown = matches!(ev, WsEvent::Shutdown);
                        let json = serde_json::to_string(&ev).unwrap_or_default();
                        let _ = socket.send(Message::Text(json.into())).await;
                        if is_shutdown {
                            return;
                        }
                    }
                    WsOutput::Completed(rel_path) => pending_completed.push(rel_path),
                    WsOutput::Skip => {}
                }
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    _ => {}
                }
            }
        }
    }
}
