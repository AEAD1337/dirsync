use crate::gui::handlers::plan_to_summary;
use crate::gui::state::AppState;
use crate::progress::{ProgressEvent, ScanPhase};

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
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use serde::Serialize;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;

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
        speed_mbps: f64,
        elapsed_secs: u64,
        eta_secs: Option<u64>,
        ops_done: usize,
        ops_total: usize,
        status: String,
    },
    /// Every status transition, pushed as it happens. The 100 ms tick above
    /// only *samples* the status, so a state the engine passes through inside
    /// one tick window is invisible to the client: and the frontend needs to
    /// see the `previewing` → `idle` edge to know its preview ended (a
    /// cancelled preview produces no `plan_ready` and no `error_occurred`).
    StatusChanged {
        status: String,
    },
    ErrorOccurred {
        path: String,
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
    /// Sent when preview finishes. Carries the full plan so the frontend can
    /// populate the op tables without a second round-trip.
    DriveMode {
        hdd: bool,
    },
    PlanReady {
        ops: Vec<crate::gui::handlers::OpEntry>,
        copy_count: usize,
        move_count: usize,
        delete_count: usize,
        overwrite_count: usize,
        identical_count: usize,
        symlink_count: usize,
        total_bytes: u64,
        total_ops: usize,
        src_dir_sizes: std::collections::HashMap<String, u64>,
    },
    LogEntry {
        level: String,
        message: String,
        run: u32,
    },
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
    state.progress.emit(ProgressEvent::Shutdown);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = state.shutdown_tx.send(true);
}

/// The wire form of a status: the serde rename the frontend's `SyncStatus`
/// union is written against.
fn status_str(status: &crate::progress::SyncStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
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
                    speed_mbps: p.speed_mbps(),
                    elapsed_secs: p.elapsed_secs(),
                    eta_secs: p.eta_secs(),
                    ops_done: p.ops_done.load(std::sync::atomic::Ordering::Relaxed),
                    ops_total: p.ops_total.load(std::sync::atomic::Ordering::Relaxed),
                    status: status_str(&status),
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
                let ev = match event {
                    Ok(ProgressEvent::StatusChanged { status }) =>
                        Some(WsEvent::StatusChanged { status: status_str(&status) }),

                    Ok(ProgressEvent::FileError { name, message }) => {
                        Some(WsEvent::ErrorOccurred { path: name, message })
                    }

                    Ok(ProgressEvent::FileDone { .. }) => None,

                    Ok(ProgressEvent::OpDone { path, .. }) => {
                        let dst_root = state.last_plan.read().unwrap()
                            .as_ref()
                            .map(|p| p.dst_root.clone());
                        let rel_path = dst_root
                            .map(|root| crate::paths::rel_to_root(std::path::Path::new(&path), &root))
                            .unwrap_or(path);
                        pending_completed.push(rel_path);
                        None
                    }

                    Ok(ProgressEvent::ScanUpdate { side, file_count }) =>
                        Some(WsEvent::ScanUpdate { side, file_count }),

                    Ok(ProgressEvent::ScanProgress { phase, path }) => {
                        let phase_str = match &phase {
                            ScanPhase::Walking { side } => format!("walking_{side}"),
                            ScanPhase::Hashing => "hashing".to_owned(),
                            ScanPhase::Planning => "planning".to_owned(),
                        };
                        Some(WsEvent::ScanProgress { phase: phase_str, path })
                    }

                    Ok(ProgressEvent::DriveMode { hdd }) =>
                        Some(WsEvent::DriveMode { hdd }),

                    Ok(ProgressEvent::PlanReady) => {
                        // Read the plan and send it in full so the frontend
                        // can populate the tables without a separate HTTP call.
                        let guard = state.last_plan.read().unwrap();
                        guard.as_ref().map(|plan| {
                            let s = plan_to_summary(plan);
                            WsEvent::PlanReady {
                                ops: s.ops,
                                copy_count: s.copy_count,
                                move_count: s.move_count,
                                delete_count: s.delete_count,
                                overwrite_count: s.overwrite_count,
                                identical_count: s.identical_count,
                                symlink_count: s.symlink_count,
                                total_bytes: s.total_bytes,
                                total_ops: s.total_ops,
                                src_dir_sizes: s.src_dir_sizes,
                            }
                        })
                    }

                    Ok(ProgressEvent::Shutdown) => Some(WsEvent::Shutdown),

                    Ok(ProgressEvent::LogEntry(entry)) => {
                        // The ring buffer is filled by a single server-side
                        // task (see gui::server::start), not here: writing it
                        // per connection duplicated every entry once per open
                        // tab and buffered nothing at all with no tab open.
                        let level = serde_json::to_value(&entry.level)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default();
                        Some(WsEvent::LogEntry {
                            level,
                            message: entry.message,
                            run: entry.run,
                        })
                    }

                    Err(_) => None,
                    _ => None,
                };

                if let Some(ev) = ev {
                    let is_shutdown = matches!(ev, WsEvent::Shutdown);
                    let json = serde_json::to_string(&ev).unwrap_or_default();
                    let _ = socket.send(Message::Text(json.into())).await;
                    if is_shutdown {
                        return;
                    }
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
