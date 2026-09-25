use crate::gui::{assets::static_handler, handlers, state::AppState, ws::ws_handler};
use crate::progress::ProgressEvent;
use axum::{
    Router,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use std::net::SocketAddr;
use std::sync::Arc;

/// A fresh per-launch session token: 128 random bits, lowercase hex.
pub fn new_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS random number generator unavailable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison, so response timing cannot leak the token.
fn token_matches(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// The token a request carries: the `X-Dirsync-Token` header (fetch), or a
/// `token` query parameter (the WebSocket, whose browser API cannot set
/// headers).
fn request_token(request: &Request) -> Option<&str> {
    if let Some(h) = request.headers().get("x-dirsync-token") {
        return h.to_str().ok();
    }
    request
        .uri()
        .query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
}

/// Gate every API and WebSocket request.
///
/// Three independent checks:
/// 1. `Host` must be one of the localhost addresses at `port`: closes DNS
///    rebinding, where an attacker's page sends `Host: evil.com`.
/// 2. When `Origin` is present it must be ours: closes browser CSRF.
/// 3. The per-launch session token must match. Headers are trivial to forge
///    for any local *process*, and loopback is shared by every user of the
///    machine (all Linux users, all Windows RDP / fast-user-switching
///    sessions): without the token another user could drive this server,
///    which runs as you, into deleting your files. Only the browser tab
///    opened with the token in its URL fragment knows it.
async fn require_session(
    port: u16,
    token: Arc<str>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let valid_hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let valid_origins = [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ];

    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !valid_hosts.iter().any(|v| v == host) {
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(origin) = request.headers().get("origin")
        && !valid_origins
            .iter()
            .any(|v| v == origin.to_str().unwrap_or(""))
    {
        return Err(StatusCode::FORBIDDEN);
    }

    match request_token(&request) {
        Some(given) if token_matches(given.as_bytes(), token.as_bytes()) => {
            Ok(next.run(request).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// The complete HTTP surface: API and WebSocket behind `require_session`,
/// static assets without it (the page must load before it can read the
/// token from its own URL; the assets contain no secrets and change nothing).
pub fn router(state: Arc<AppState>, port: u16, token: String) -> Router {
    let token: Arc<str> = token.into();
    Router::new()
        .route(
            "/api/v1/config",
            get(handlers::get_config).put(handlers::put_config),
        )
        .route("/api/v1/preview", post(handlers::post_preview))
        .route("/api/v1/run", post(handlers::post_run))
        .route("/api/v1/pause", post(handlers::post_pause))
        .route("/api/v1/cancel", post(handlers::post_cancel))
        .route("/api/v1/browse", post(handlers::post_browse))
        .route("/api/v1/complete", post(handlers::post_complete))
        .route("/api/v1/stat", post(handlers::post_stat))
        .route("/api/v1/shutdown", post(handlers::post_shutdown))
        .route("/api/v1/plan", get(handlers::get_plan))
        .route("/api/v1/log", get(handlers::get_log))
        .route("/api/v1/system", get(handlers::get_system))
        .route("/ws", get(ws_handler))
        .layer(middleware::from_fn(move |req, next| {
            require_session(port, token.clone(), req, next)
        }))
        .with_state(state)
        .fallback(static_handler)
}

/// Buffer a log entry for `GET /log` (the single server-side writer).
fn record_log(state: &AppState, entry: crate::progress::LogEntry) {
    let mut buf = state.log_buffer.lock().unwrap();
    crate::gui::ws::push_log_entry(&mut buf, entry);
}

/// Record that the log consumer fell `missed` events behind the broadcast
/// channel, so the buffered log shows the gap instead of hiding it.
pub fn record_lag(state: &AppState, missed: u64) {
    let entry = crate::gui::ws::lag_notice(missed, state.progress.current_run());
    eprintln!("[WARN ] {}", entry.message);
    record_log(state, entry);
}

pub async fn start(state: Arc<AppState>, port: u16) -> anyhow::Result<()> {
    // Bind first and derive everything user-visible - printed URL, browser
    // open, Host/Origin allowlist - from the *actual* bound address, so a
    // port the OS reassigns can never produce an unreachable UI.
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let addr = listener.local_addr()?;
    let port = addr.port();

    let token = new_token();
    let app = router(state.clone(), port, token.clone());

    // The token travels in the fragment: browsers never send it to the
    // server in a request line or a Referer, and the page moves it into
    // sessionStorage and strips it from the address bar.
    let url = format!("http://{addr}/#t={token}");
    println!("dirsync GUI running at {url}");

    // Open browser in background
    let url2 = url.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Err(e) = open::that(&url2) {
            eprintln!("Could not open browser: {e}");
        }
    });

    // Listen for OS signals and turn them into an orderly shutdown so the
    // frontend receives a WsEvent::Shutdown before the connection drops.
    let state_sig = state.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        state_sig.request_shutdown().await;
    });

    // Mirror log entries to the console so `--gui` runs are debuggable without
    // needing a browser open, and fill the ring buffer that GET /api/v1/log
    // serves. Both happen here, in one task, rather than per WebSocket
    // connection: otherwise two open tabs duplicated every entry and a
    // headless period buffered nothing.
    let mut console_rx = state.progress.subscribe();
    let mut console_shutdown_rx = state.shutdown_tx.subscribe();
    let log_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                ev = console_rx.recv() => {
                    match ev {
                        Ok(ProgressEvent::LogEntry(entry)) => {
                            record_log(&log_state, entry.clone());
                            let prefix = match entry.level {
                                crate::progress::LogLevel::Info    => "INFO ",
                                crate::progress::LogLevel::Warning => "WARN ",
                                crate::progress::LogLevel::Error   => "ERROR",
                            };
                            if matches!(entry.level, crate::progress::LogLevel::Error) {
                                eprintln!("[{prefix}] {}", entry.message);
                            } else {
                                println!("[{prefix}] {}", entry.message);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                            record_lag(&log_state, missed);
                        }
                        Err(_) | Ok(ProgressEvent::Shutdown) => break,
                        _ => {}
                    }
                }
                _ = console_shutdown_rx.changed() => break,
            }
        }
    });

    let mut shutdown_rx = state.shutdown_tx.subscribe();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // Wait until the watch channel flips to true.
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                if shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await?;

    // request_shutdown set the cancel flag; give the executor the chance to
    // observe it and write its final status instead of dropping it mid-op.
    let run_task = state.run_task.lock().unwrap().take();
    if let Some(handle) = run_task {
        let _ = handle.await;
    }
    Ok(())
}

/// Resolves when SIGINT (Ctrl+C) or SIGTERM is received.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).unwrap_or_else(|_| {
            // If we can't register SIGTERM, just use a future that never resolves.
            signal(SignalKind::hangup()).expect("failed to install signal handler")
        });
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}
