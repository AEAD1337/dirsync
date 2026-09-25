//! The HTTP boundary: the token and same-origin middleware in front of every
//! API route. This is the whole security perimeter of a server that can
//! delete files, so it is tested on the real router.
#![cfg(feature = "gui")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dirsync::config::AppConfig;
use dirsync::gui::server::router;
use dirsync::gui::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;

const PORT: u16 = 7373;
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn app(dir: &TempDir) -> axum::Router {
    let config = AppConfig {
        path: Some(dir.path().join("config.toml")),
        ..AppConfig::default()
    };
    let (state, _rx) = AppState::new(config, true, false);
    router(state, PORT, TOKEN.to_owned())
}

fn get(uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .uri(uri)
        .header("host", format!("127.0.0.1:{PORT}"))
}

async fn status(app: axum::Router, req: Request<Body>) -> StatusCode {
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn an_api_call_without_the_token_is_unauthorized() {
    let dir = TempDir::new().unwrap();
    // Another local user can forge Host and Origin, but cannot know the token.
    let req = get("/api/v1/config").body(Body::empty()).unwrap();
    assert_eq!(status(app(&dir), req).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_api_call_with_a_wrong_token_is_unauthorized() {
    let dir = TempDir::new().unwrap();
    let req = get("/api/v1/config")
        .header("x-dirsync-token", "ffffffffffffffffffffffffffffffff")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(app(&dir), req).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_api_call_with_the_token_succeeds() {
    let dir = TempDir::new().unwrap();
    let req = get("/api/v1/config")
        .header("x-dirsync-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(app(&dir), req).await, StatusCode::OK);
}

#[tokio::test]
async fn the_websocket_route_requires_the_token_too() {
    let dir = TempDir::new().unwrap();
    let req = get("/ws").body(Body::empty()).unwrap();
    assert_eq!(status(app(&dir), req).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_foreign_host_is_rejected_even_with_the_token() {
    let dir = TempDir::new().unwrap();
    // DNS rebinding: the page is served from evil.example under our IP.
    let req = Request::builder()
        .uri("/api/v1/config")
        .header("host", "evil.example")
        .header("x-dirsync-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(app(&dir), req).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_cross_origin_post_is_rejected_even_with_the_token() {
    let dir = TempDir::new().unwrap();
    let req = get("/api/v1/cancel")
        .method("POST")
        .header("origin", "https://evil.example")
        .header("x-dirsync-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(app(&dir), req).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_same_origin_post_with_the_token_is_accepted() {
    let dir = TempDir::new().unwrap();
    let req = get("/api/v1/cancel")
        .method("POST")
        .header("origin", format!("http://127.0.0.1:{PORT}"))
        .header("x-dirsync-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    assert!(status(app(&dir), req).await.is_success());
}

#[tokio::test]
async fn a_bare_get_cannot_burn_the_one_shot_auto_preview() {
    let dir = TempDir::new().unwrap();
    let app = app(&dir);
    // What `<img src=".../api/v1/system">` on any website sends.
    let forged = get("/api/v1/system").body(Body::empty()).unwrap();
    assert_eq!(status(app.clone(), forged).await, StatusCode::UNAUTHORIZED);

    let real = get("/api/v1/system")
        .header("x-dirsync-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(real).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["auto_preview"], true);
}

#[tokio::test]
async fn static_assets_need_no_token() {
    let dir = TempDir::new().unwrap();
    // The page must load to read the token from its own URL fragment.
    let req = get("/").body(Body::empty()).unwrap();
    assert_ne!(status(app(&dir), req).await, StatusCode::UNAUTHORIZED);
}

#[test]
fn every_launch_gets_a_fresh_unguessable_token() {
    let a = dirsync::gui::server::new_token();
    let b = dirsync::gui::server::new_token();
    assert_eq!(a.len(), 32);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b);
}
