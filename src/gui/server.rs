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
use std::time::Duration;

/// Reject requests that fail the same-origin check.
///
/// Two independent defences:
/// 1. `Host` header must be one of the known localhost addresses at `port`.
///    This closes DNS-rebinding: an attacker whose page is served from
///    `evil.com` sends `Host: evil.com`, which is not in the allowlist.
/// 2. When `Origin` is present it must match a known localhost origin.
///    When `Origin` is absent on a state-changing method (`POST`/`PUT`),
///    the request is rejected: closing the local-process scripting vector.
async fn require_same_origin(
    port: u16,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let valid_hosts = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    let valid_origins = [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        format!("http://[::1]:{port}"),
    ];

    // Always validate Host: this is the primary DNS-rebinding defence.
    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !valid_hosts.iter().any(|v| v == host) {
        return Err(StatusCode::FORBIDDEN);
    }

    match request.headers().get("origin") {
        Some(origin) => {
            if !valid_origins
                .iter()
                .any(|v| v == origin.to_str().unwrap_or(""))
            {
                return Err(StatusCode::FORBIDDEN);
            }
        }
        None => {
            // Browsers always send Origin on cross-origin state-changing
            // requests. A missing Origin on POST/PUT means a non-browser
            // local process; reject it to limit the local attack surface.
            let method = request.method();
            if method == axum::http::Method::POST || method == axum::http::Method::PUT {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }

    Ok(next.run(request).await)
}

pub async fn start(state: Arc<AppState>, port: u16) -> anyhow::Result<()> {
    // Bind first and derive everything user-visible - printed URL, browser
    // open, Host/Origin allowlist - from the *actual* bound address, so a
    // port the OS reassigns can never produce an unreachable UI.
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let addr = listener.local_addr()?;
    let port = addr.port();

    // API and WebSocket routes are protected by the same-origin middleware.
    // Static assets use a separate router without it (no state mutations possible).
    let api = Router::new()
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
            require_same_origin(port, req, next)
        }))
        .with_state(state.clone());

    let app = api.fallback(static_handler);

    let url = format!("http://{addr}");
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
        state_sig.progress.emit(ProgressEvent::Shutdown);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = state_sig.shutdown_tx.send(true);
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
                            {
                                let mut buf = log_state.log_buffer.lock().unwrap();
                                crate::gui::ws::push_log_entry(&mut buf, entry.clone());
                            }
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
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
