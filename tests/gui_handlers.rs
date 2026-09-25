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
use dirsync::gui::ws::{WsOutput, ws_event_for};
use dirsync::progress::{ProgressEvent, SyncStatus};
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
        walk_errors: vec![],
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

/// Shutdown has to stop a sync that is really in progress, not merely raise
/// a flag: the run, already working through its plan, ends Cancelled before
/// it finished, and the server's shutdown watch flips.
#[tokio::test(flavor = "multi_thread")]
async fn request_shutdown_cancels_a_running_sync() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let (src, dst) = endpoints(&dir);
    let total = 1000;
    let ops = (0..total)
        .map(|i| SyncOp::MkDir {
            path: dst.join(format!("d{i}")),
        })
        .collect();
    *state.last_plan.write().unwrap() = Some(plan_for(src.clone(), dst.clone(), ops));
    let mut rx = state.progress.subscribe();
    let mut shutdown_rx = state.shutdown_tx.subscribe();

    handlers::post_run(State(state.clone()), Json(run_request(&src, &dst)))
        .await
        .unwrap();
    // Wait until the executor has actually completed an op.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(ProgressEvent::OpDone { .. }) = rx.recv().await {
                break;
            }
        }
    })
    .await
    .expect("the run never started");

    state.request_shutdown().await;
    let handle = state.run_task.lock().unwrap().take().unwrap();
    tokio::time::timeout(Duration::from_secs(20), handle)
        .await
        .expect("the run did not stop")
        .unwrap();

    assert_eq!(
        *state.progress.status.read().unwrap(),
        SyncStatus::Cancelled
    );
    let made = std::fs::read_dir(&dst).unwrap().count();
    assert!(
        made < total,
        "the run finished its plan despite the shutdown"
    );
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

#[test]
fn current_dir_is_none_when_no_file_is_being_copied() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    *state.last_plan.write().unwrap() = Some(plan_for(
        PathBuf::from("/src"),
        PathBuf::from("/dst"),
        vec![],
    ));

    assert_eq!(dirsync::gui::ws::current_dir_rel(&state), None);
}

#[test]
fn current_dir_is_the_dst_relative_parent_of_the_file_being_copied() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    *state.last_plan.write().unwrap() = Some(plan_for(
        PathBuf::from("/src"),
        PathBuf::from("/dst"),
        vec![],
    ));
    *state.progress.current_file_dst.write().unwrap() =
        Some(PathBuf::from("/dst/photos/2024/a.jpg"));

    assert_eq!(
        dirsync::gui::ws::current_dir_rel(&state).as_deref(),
        Some("photos/2024"),
        "the GUI lights this row for the whole of a chunked copy"
    );
}

#[test]
fn current_dir_is_none_for_a_file_directly_in_the_destination_root() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    *state.last_plan.write().unwrap() = Some(plan_for(
        PathBuf::from("/src"),
        PathBuf::from("/dst"),
        vec![],
    ));
    *state.progress.current_file_dst.write().unwrap() = Some(PathBuf::from("/dst/a.jpg"));

    // No directory row exists for a top-level file, so there is nothing to light.
    assert_eq!(dirsync::gui::ws::current_dir_rel(&state), None);
}

#[test]
fn current_dir_is_none_for_a_target_outside_the_destination_root() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    *state.last_plan.write().unwrap() = Some(plan_for(
        PathBuf::from("/src"),
        PathBuf::from("/dst"),
        vec![],
    ));
    *state.progress.current_file_dst.write().unwrap() = Some(PathBuf::from("/elsewhere/a.jpg"));

    // Never hand the client an absolute path it cannot match against a row.
    assert_eq!(dirsync::gui::ws::current_dir_rel(&state), None);
}

// --- Run outcome reporting ---

fn run_request(src: &std::path::Path, dst: &std::path::Path) -> RunRequest {
    RunRequest {
        dry_run: false,
        skip_prefixes: vec![],
        src: src.to_string_lossy().into_owned(),
        dst: dst.to_string_lossy().into_owned(),
    }
}

fn sent_json(out: WsOutput) -> serde_json::Value {
    match out {
        WsOutput::Send(ev) => serde_json::to_value(ev).unwrap(),
        _ => panic!("expected an event to send"),
    }
}

#[test]
fn a_file_error_reaches_the_client_as_a_dst_relative_path() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let dst = dir.path().join("dst");
    *state.last_plan.write().unwrap() = Some(plan_for(dir.path().join("src"), dst.clone(), vec![]));

    let out = ws_event_for(
        &state,
        ProgressEvent::FileError {
            name: dst.join("d").join("ro.txt").display().to_string(),
            message: "Access is denied".into(),
        },
    );

    // Same form as op.rel_path, so the client can mark the failed row.
    let json = sent_json(out);
    assert_eq!(json["type"], "error_occurred");
    assert_eq!(json["path"], "d/ro.txt");
}

#[test]
fn a_failed_preview_is_its_own_event_not_a_file_error() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);

    let json = sent_json(ws_event_for(
        &state,
        ProgressEvent::PreviewFailed {
            message: "cannot read SRC".into(),
        },
    ));

    assert_eq!(json["type"], "preview_failed");
    assert_eq!(json["message"], "cannot read SRC");
}

#[tokio::test]
async fn the_error_summary_is_the_last_log_line_of_a_failed_run() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    let ops = (0..3)
        .map(|i| SyncOp::Copy {
            src: src.join(format!("missing{i}.txt")),
            dst: dst.join(format!("missing{i}.txt")),
            size: 1,
            hash: None,
        })
        .collect();
    *state.last_plan.write().unwrap() = Some(plan_for(src.clone(), dst.clone(), ops));
    let mut rx = state.progress.subscribe();

    handlers::post_run(State(state.clone()), Json(run_request(&src, &dst)))
        .await
        .unwrap();
    let handle = state.run_task.lock().unwrap().take().unwrap();
    handle.await.unwrap();

    let mut errors = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ProgressEvent::LogEntry(e) = ev
            && matches!(e.level, dirsync::progress::LogLevel::Error)
        {
            errors.push(e.message);
        }
    }
    // A burst drops the *oldest* events on a lagging receiver: the summary
    // must not be the one line that is guaranteed to go first.
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(errors[3].contains("3 file(s) had errors"), "{errors:?}");
}

#[tokio::test]
async fn a_lagging_log_consumer_records_how_much_it_missed() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);

    dirsync::gui::server::record_lag(&state, 42);

    let buf = state.log_buffer.lock().unwrap();
    let last = buf.back().expect("a notice must be buffered");
    assert!(last.message.contains("42"), "{}", last.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_run_drops_its_plan() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    let ops = (0..400)
        .map(|i| SyncOp::MkDir {
            path: dst.join(format!("d{i}")),
        })
        .collect();
    *state.last_plan.write().unwrap() = Some(plan_for(src.clone(), dst.clone(), ops));

    handlers::post_run(State(state.clone()), Json(run_request(&src, &dst)))
        .await
        .unwrap();
    state.cancel_tx.send_replace(true);
    let handle = state.run_task.lock().unwrap().take().unwrap();
    handle.await.unwrap();

    assert_eq!(
        *state.progress.status.read().unwrap(),
        SyncStatus::Cancelled
    );
    // Replaying it would redo moves and deletes that already ran: they
    // fail as NotFound against the correct files.
    assert!(state.last_plan.read().unwrap().is_none());
}

#[tokio::test]
async fn a_preview_stores_its_plan_before_it_reports_idle() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(src.join("a.txt"), b"a").unwrap();
    let mut rx = state.progress.subscribe();

    let body = handlers::PreviewRequest {
        src: format!("{}/", src.display()),
        dst: format!("{}/", dst.display()),
        excludes: vec![],
    };
    let _accepted = handlers::post_preview(State(state.clone()), Json(body))
        .await
        .unwrap();
    wait_for(&state, SyncStatus::Idle).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut order = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ProgressEvent::PlanReady => order.push("plan_ready"),
            ProgressEvent::StatusChanged {
                status: SyncStatus::Idle,
            } => order.push("idle"),
            _ => {}
        }
    }
    // Idle before plan_ready showed "Nothing to sync" and re-enabled
    // Preview while the plan was still in flight.
    assert_eq!(order, vec!["plan_ready", "idle"]);
}

#[tokio::test]
async fn browse_skips_entries_whose_names_are_not_unicode() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("fine")).unwrap();
    #[cfg(windows)]
    let odd = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xD800, b'x' as u16])
    };
    #[cfg(unix)]
    let odd = {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(b"bad\xff").to_os_string()
    };
    if std::fs::create_dir(dir.path().join(&odd)).is_err() {
        eprintln!("skipped: filesystem refuses non-Unicode names");
        return;
    }
    let state = state_with_config(&dir, AppConfig::default(), false);

    let body: handlers::BrowseRequest = serde_json::from_value(serde_json::json!({
        "path": dir.path(),
        "dir_only": true,
    }))
    .unwrap();
    let Json(resp) = handlers::post_browse(State(state), Json(body))
        .await
        .expect("one odd name must not fail the whole listing");

    let names: Vec<String> = resp.entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, vec!["fine"]);
    // Serializing must work too: that is where the 500 came from.
    serde_json::to_string(&resp).unwrap();
}

// --- Run and preview claims ---

/// Fresh SRC and DST directories under `dir`.
fn endpoints(dir: &TempDir) -> (PathBuf, PathBuf) {
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    (src, dst)
}

fn preview_request(src: &std::path::Path, dst: &std::path::Path) -> handlers::PreviewRequest {
    handlers::PreviewRequest {
        src: format!("{}/", src.display()),
        dst: format!("{}/", dst.display()),
        excludes: vec![],
    }
}

/// A preview must not start while a run owns the status or another preview
/// is in flight. Rejecting it must leave everything as it was: the status,
/// the stored plan, and the executor's cancel flag (a preview resets the
/// shared control channels when it starts, which would erase a pending
/// cancel of the running sync).
#[tokio::test]
async fn post_preview_is_rejected_while_a_run_or_a_preview_is_active() {
    for busy in [
        SyncStatus::Running,
        SyncStatus::Paused,
        SyncStatus::Previewing,
    ] {
        let dir = TempDir::new().unwrap();
        let state = state_with_config(&dir, AppConfig::default(), false);
        let (src, dst) = endpoints(&dir);
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        let stored = plan_for(src.clone(), dst.clone(), vec![]);
        *state.last_plan.write().unwrap() = Some(stored);
        *state.progress.status.write().unwrap() = busy.clone();
        state.cancel_tx.send_replace(true);

        let (code, _) =
            handlers::post_preview(State(state.clone()), Json(preview_request(&src, &dst)))
                .await
                .expect_err("a busy state must reject the preview");

        assert_eq!(code, StatusCode::CONFLICT, "{busy:?}");
        assert_eq!(*state.progress.status.read().unwrap(), busy);
        assert!(
            *state.cancel_tx.borrow(),
            "{busy:?}: a rejected preview reset the run's cancel flag"
        );
        assert!(state.last_plan.read().unwrap().is_some(), "{busy:?}");
        // Nothing was spawned that could replace the plan later either.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*state.progress.status.read().unwrap(), busy);
        assert!(
            state
                .last_plan
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .ops
                .is_empty(),
            "{busy:?}"
        );
    }
}

/// A MkDir plan for `dst` whose run would visibly create `dst/new-dir`.
fn mkdir_plan(src: &std::path::Path, dst: &std::path::Path) -> SyncPlan {
    plan_for(
        src.to_path_buf(),
        dst.to_path_buf(),
        vec![SyncOp::MkDir {
            path: dst.join("new-dir"),
        }],
    )
}

#[tokio::test]
async fn post_run_is_rejected_while_a_preview_is_in_progress() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let (src, dst) = endpoints(&dir);
    *state.last_plan.write().unwrap() = Some(mkdir_plan(&src, &dst));
    *state.progress.status.write().unwrap() = SyncStatus::Previewing;

    let (code, _) = handlers::post_run(State(state.clone()), Json(run_request(&src, &dst)))
        .await
        .expect_err("a run must not start over a preview");

    assert_eq!(code, StatusCode::CONFLICT);
    // The preview still owns the status: the rejection must not release it.
    assert_eq!(
        *state.progress.status.read().unwrap(),
        SyncStatus::Previewing
    );
    assert!(state.run_task.lock().unwrap().is_none());
    assert!(state.last_plan.read().unwrap().is_some());
    assert!(!dst.join("new-dir").exists());
}

#[tokio::test]
async fn post_run_is_rejected_when_src_or_dst_differ_from_the_plan() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let (src, dst) = endpoints(&dir);
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    *state.last_plan.write().unwrap() = Some(mkdir_plan(&src, &dst));

    for (req_src, req_dst) in [(&other, &dst), (&src, &other)] {
        let (code, _) =
            handlers::post_run(State(state.clone()), Json(run_request(req_src, req_dst)))
                .await
                .expect_err("a stale plan must not run");

        assert_eq!(code, StatusCode::CONFLICT);
        // The run claim is released again, so a new preview can start.
        assert_eq!(*state.progress.status.read().unwrap(), SyncStatus::Idle);
        assert!(state.run_task.lock().unwrap().is_none());
        assert!(state.last_plan.read().unwrap().is_some());
        assert!(!dst.join("new-dir").exists());
    }
}

#[tokio::test]
async fn post_run_without_a_plan_is_a_bad_request() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let (src, dst) = endpoints(&dir);

    let (code, _) = handlers::post_run(State(state.clone()), Json(run_request(&src, &dst)))
        .await
        .expect_err("there is nothing to run");

    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(*state.progress.status.read().unwrap(), SyncStatus::Idle);
    assert!(state.run_task.lock().unwrap().is_none());
}

/// "Skip this directory" must change what executes, not just what is shown:
/// the skipped subtree gets no writes, the rest of the plan runs, and
/// deletes below the skipped directory still run (skipping suppresses
/// writes, it does not cancel cleanup).
#[tokio::test]
async fn post_run_skip_prefixes_drop_the_skipped_writes_from_the_run() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let (src, dst) = endpoints(&dir);
    for rel in ["keep/a.txt", "skip/b.txt", "skip/deeper/c.txt"] {
        let path = src.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, rel.as_bytes()).unwrap();
    }
    std::fs::create_dir_all(dst.join("skip")).unwrap();
    std::fs::write(dst.join("skip/orphan.txt"), b"orphan").unwrap();
    let copy = |rel: &str| SyncOp::Copy {
        src: src.join(rel),
        dst: dst.join(rel),
        size: rel.len() as u64,
        hash: None,
    };
    let plan = plan_for(
        src.clone(),
        dst.clone(),
        vec![
            SyncOp::MkDir {
                path: dst.join("keep"),
            },
            SyncOp::MkDir {
                path: dst.join("skip/deeper"),
            },
            copy("keep/a.txt"),
            copy("skip/b.txt"),
            copy("skip/deeper/c.txt"),
            SyncOp::Delete {
                path: dst.join("skip/orphan.txt"),
                size: 6,
            },
        ],
    );
    *state.last_plan.write().unwrap() = Some(plan);

    let body = RunRequest {
        skip_prefixes: vec!["skip/".into()],
        ..run_request(&src, &dst)
    };
    handlers::post_run(State(state.clone()), Json(body))
        .await
        .unwrap();
    let handle = state.run_task.lock().unwrap().take().unwrap();
    handle.await.unwrap();

    assert_eq!(*state.progress.status.read().unwrap(), SyncStatus::Done);
    assert_eq!(
        std::fs::read(dst.join("keep/a.txt")).unwrap(),
        b"keep/a.txt"
    );
    assert!(!dst.join("skip/b.txt").exists(), "a skipped copy ran");
    assert!(!dst.join("skip/deeper").exists(), "a skipped mkdir ran");
    assert!(
        !dst.join("skip/orphan.txt").exists(),
        "deletes below a skipped directory still run"
    );
}

/// A dry run changes nothing, so its plan stays runnable for real.
#[tokio::test]
async fn a_dry_run_keeps_the_plan_and_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let state = state_with_config(&dir, AppConfig::default(), false);
    let (src, dst) = endpoints(&dir);
    *state.last_plan.write().unwrap() = Some(mkdir_plan(&src, &dst));

    let body = RunRequest {
        dry_run: true,
        ..run_request(&src, &dst)
    };
    handlers::post_run(State(state.clone()), Json(body))
        .await
        .unwrap();
    let handle = state.run_task.lock().unwrap().take().unwrap();
    handle.await.unwrap();

    assert_eq!(*state.progress.status.read().unwrap(), SyncStatus::Done);
    assert!(!dst.join("new-dir").exists(), "a dry run wrote to DST");
    let kept = state.last_plan.read().unwrap();
    let kept = kept.as_ref().expect("a dry run must keep its plan");
    assert_eq!(kept.ops.len(), 1);
}
