//! GUI handler behaviour that the previous reviews found only by inspection:
//! the run lifecycle around `last_plan`, config merging, one-shot
//! auto-preview, shutdown cancelling a run, and the static fallback.
#![cfg(feature = "gui")]

use axum::extract::{Json, State};
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use dirsync::config::{AppConfig, Theme};
use dirsync::gui::assets::static_handler;
use dirsync::gui::handlers::{self, ConfigPatch, RunRequest, plan_to_summary};
use dirsync::gui::state::AppState;
use dirsync::progress::SyncStatus;
use dirsync::sync::planner::{SyncOp, SyncPlan};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn plan_for(src_root: PathBuf, dst_root: PathBuf, ops: Vec<SyncOp>) -> SyncPlan {
    SyncPlan {
        ops,
        total_bytes: 0,
        copy_count: 0,
        move_count: 0,
        delete_count: 0,
        overwrite_count: 0,
        identical_count: 0,
        touch_count: 0,
        symlink_count: 0,
        src_root,
        dst_root,
        hdd: false,
        dir_blocked_targets: vec![],
    }
}

/// A state whose config is pinned to a temp file so no test touches the
/// user's real config.toml.
fn state_with_config(dir: &TempDir, mut config: AppConfig, auto_preview: bool) -> Arc<AppState> {
    config.path = Some(dir.path().join("config.toml"));
    let (state, _rx) = AppState::new(config, auto_preview, false);
    state
}

async fn wait_for(state: &AppState, status: SyncStatus) {
    for _ in 0..100 {
        if *state.progress.status.read().unwrap() == status {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("status never reached {status:?}");
}

#[tokio::test]
async fn post_run_clears_the_plan_once_the_run_is_done() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    let plan = plan_for(
        src.clone(),
        dst.clone(),
        vec![SyncOp::MkDir {
            path: dst.join("new-dir"),
        }],
    );
    *state.last_plan.write().unwrap() = Some(plan);

    let body = RunRequest {
        dry_run: false,
        skip_prefixes: vec![],
        src: src.to_string_lossy().into_owned(),
        dst: dst.to_string_lossy().into_owned(),
    };
    let code = handlers::post_run(State(state.clone()), Json(body))
        .await
        .expect("run should be accepted");
    assert_eq!(code, StatusCode::ACCEPTED);

    // The executor's handle is kept so shutdown can wait for it.
    let handle = state
        .run_task
        .lock()
        .unwrap()
        .take()
        .expect("post_run must store the executor handle");
    handle.await.unwrap();
    wait_for(&state, SyncStatus::Done).await;

    assert!(
        state.last_plan.read().unwrap().is_none(),
        "a consumed plan must not be runnable again"
    );
}

#[tokio::test]
async fn put_config_merges_the_patch_and_keeps_server_side_paths() {
    let dir = TempDir::new().unwrap();
    let config = AppConfig {
        last_src: Some(PathBuf::from("/new/src")),
        last_dst: Some(PathBuf::from("/new/dst")),
        exclude_patterns: vec!["*.tmp".into()],
        ..AppConfig::default()
    };
    let state = state_with_config(&dir, config, false);

    let patch = ConfigPatch {
        theme: Some(Theme::Dark),
        ..ConfigPatch::default()
    };
    let Json(saved) = handlers::put_config(State(state.clone()), Json(patch))
        .await
        .expect("patch should be accepted");

    assert_eq!(saved.theme, Theme::Dark);
    assert_eq!(saved.last_src, Some(PathBuf::from("/new/src")));
    assert_eq!(saved.exclude_patterns, vec!["*.tmp"]);
    // Persisted to the pinned path, not the user's config.
    let on_disk = AppConfig::load_from(&dir.path().join("config.toml"));
    assert_eq!(on_disk.theme, Theme::Dark);
    assert_eq!(on_disk.last_src, Some(PathBuf::from("/new/src")));
}

#[tokio::test]
async fn put_config_rejects_an_invalid_port() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let patch = ConfigPatch {
        port: Some(80),
        ..ConfigPatch::default()
    };
    let (code, _) = handlers::put_config(State(state), Json(patch))
        .await
        .unwrap_err();
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_system_reports_auto_preview_only_once() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), true);

    let Json(first) = handlers::get_system(State(state.clone())).await;
    let Json(second) = handlers::get_system(State(state)).await;

    assert_eq!(first["auto_preview"], true);
    assert_eq!(
        second["auto_preview"], false,
        "a reload must not trigger another full scan"
    );
}

#[tokio::test]
async fn request_shutdown_cancels_a_running_sync() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let cancel_rx = state.cancel_rx();
    let mut shutdown_rx = state.shutdown_tx.subscribe();

    state.request_shutdown().await;

    assert!(*cancel_rx.borrow(), "an in-flight run must be told to stop");
    assert!(*shutdown_rx.borrow_and_update());
}

#[test]
fn plan_to_summary_lists_touch_ops() {
    let plan = plan_for(
        PathBuf::from("/src"),
        PathBuf::from("/dst"),
        vec![SyncOp::TouchMtime {
            src: PathBuf::from("/src/a.txt"),
            dst: PathBuf::from("/dst/a.txt"),
        }],
    );
    let summary = plan_to_summary(&plan);
    assert_eq!(summary.ops.len(), 1);
    assert_eq!(summary.ops[0].kind, "touch");
    assert_eq!(summary.ops[0].rel_path, "a.txt");
    assert_eq!(summary.total_ops, 1);
}

#[tokio::test]
async fn static_handler_returns_404_under_the_api_prefix() {
    let uri: Uri = "/api/v1/does-not-exist".parse().unwrap();
    let response = static_handler(uri).await.into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
