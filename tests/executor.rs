use dirsync::config::AppConfig;
use dirsync::progress::{SyncStatus, new_progress_channel};
use dirsync::sync::SyncEngine;
use dirsync::sync::executor::{ExecuteOptions, execute};
use dirsync::sync::planner::{SyncOp, SyncPlan};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::watch;

fn bare_plan(dst_root: std::path::PathBuf, ops: Vec<SyncOp>) -> SyncPlan {
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
        src_root: std::path::PathBuf::from("/src"),
        dst_root,
        hdd: false,
        src_dir_sizes: HashMap::new(),
        dir_blocked_targets: vec![],
    }
}

// --- Safety gate ---

#[tokio::test]
async fn test_safety_gate_rejects_op_outside_dst_root() {
    let dst = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();

    // A Copy op whose dst is inside `other`, not inside `dst`.
    let rogue_dst = other.path().join("rogue.txt");
    let src_file = dst.path().join("src.txt");
    fs::write(&src_file, b"content").unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::Copy {
            src: src_file,
            dst: rogue_dst.clone(),
            size: 7,
            hash: None,
        }],
    );

    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (_, cancel_rx) = watch::channel(false);

    let skip_log = execute(
        plan,
        progress.clone(),
        ExecuteOptions {
            dry_run: false,
            hdd: false,
        },
        pause_rx,
        cancel_rx,
    )
    .await;

    assert!(!skip_log.is_empty(), "safety gate must record an error");
    assert!(!rogue_dst.exists(), "rogue file must not be written");
    assert_eq!(
        *progress.status.read().unwrap(),
        SyncStatus::Cancelled,
        "status must be Cancelled after safety gate fires"
    );
}

// --- Cancel ---

#[tokio::test]
async fn test_cancel_before_run_stops_immediately() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    for i in 0..10 {
        fs::write(src.path().join(format!("f{i}.txt")), format!("data{i}")).unwrap();
    }

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (cancel_tx, cancel_rx) = watch::channel(false);

    // Signal cancel before run starts.
    cancel_tx.send(true).unwrap();

    engine
        .run(plan, progress.clone(), false, pause_rx, cancel_rx)
        .await;

    assert_eq!(
        *progress.status.read().unwrap(),
        SyncStatus::Cancelled,
        "status must be Cancelled when cancel fires before run"
    );
}

// --- Pause / resume ---

#[tokio::test]
async fn test_pause_and_resume_completes_run() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    // Put files only in dst so the plan contains delete ops (serial phase,
    // each preceded by a wait_if_paused check: ideal for pause testing).
    for i in 0..15 {
        fs::write(dst.path().join(format!("orphan{i}.txt")), b"bye").unwrap();
    }

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();
    assert_eq!(plan.delete_count, 15);

    let (progress, _rx) = new_progress_channel();
    // Start paused so the first op hits wait_if_paused immediately.
    let (pause_tx, pause_rx) = watch::channel(true);
    let (_, cancel_rx) = watch::channel(false);

    let progress2 = progress.clone();
    let handle = tokio::spawn(async move {
        engine
            .run(plan, progress2, false, pause_rx, cancel_rx)
            .await
    });

    // Wait for status to reach Paused (up to 3 s).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if *progress.status.read().unwrap() == SyncStatus::Paused {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for Paused status"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Resume and let the run finish.
    pause_tx.send(false).unwrap();
    handle.await.unwrap();

    assert_eq!(
        *progress.status.read().unwrap(),
        SyncStatus::Done,
        "run should complete after resume"
    );
}

// --- Large-file copy (> 1 MB, triggers copy_with_progress) ---

#[tokio::test]
async fn test_large_file_copy_via_progress_path() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    // 2 MB file exceeds the SMALL_FILE threshold (1 MB) and forces the
    // chunked copy_with_progress code path.
    let large: Vec<u8> = (0u8..=255).cycle().take(2 * 1024 * 1024).collect();
    fs::write(src.path().join("large.bin"), &large).unwrap();

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(plan.copy_count, 1, "one copy op expected");

    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (_, cancel_rx) = watch::channel(false);
    engine
        .run(plan, progress.clone(), false, pause_rx, cancel_rx)
        .await;

    let copied = fs::read(dst.path().join("large.bin")).unwrap();
    assert_eq!(copied, large, "large file content must be byte-exact");
    assert_eq!(*progress.status.read().unwrap(), SyncStatus::Done);
    // Verify byte accounting fired (done_bytes should reflect the file size).
    let done = progress
        .done_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        done >= 2 * 1024 * 1024,
        "done_bytes should reflect large file transfer"
    );
}

// --- I/O error populates skip log ---

#[tokio::test]
async fn test_io_error_op_populates_skip_log() {
    let dst = TempDir::new().unwrap();

    // Delete op targeting a file that does not exist: executor will error.
    let missing = dst.path().join("ghost.txt");
    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::Delete {
            path: missing,
            size: 0,
        }],
    );

    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (_, cancel_rx) = watch::channel(false);

    let skip_log = execute(
        plan,
        progress,
        ExecuteOptions {
            dry_run: false,
            hdd: false,
        },
        pause_rx,
        cancel_rx,
    )
    .await;

    assert!(
        !skip_log.is_empty(),
        "failed delete must add an entry to the skip log"
    );
}

// --- File-over-directory replacement ---

/// SRC has a file where DST has a directory. `fs::rename` cannot replace a
/// directory, and the directory's own Delete/RmDir ops used to run two phases
/// after the copy: so the copy failed on the first run and left a
/// `.__dirsync_tmp__` file behind. The executor now hoists those cleanup ops
/// in front of the copy phase.
#[tokio::test]
async fn test_file_replaces_directory_in_one_run() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    fs::write(src.path().join("x"), b"iamafile").unwrap();
    fs::create_dir(dst.path().join("x")).unwrap();
    fs::write(dst.path().join("x").join("inner.txt"), b"inner").unwrap();

    let engine = SyncEngine::new(
        src.path().to_path_buf(),
        dst.path().to_path_buf(),
        Arc::new(AppConfig::default()),
    );
    let plan = engine.preview(None, None).await.unwrap();

    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (_, cancel_rx) = watch::channel(false);
    let skip_log = engine.run(plan, progress, false, pause_rx, cancel_rx).await;

    assert!(
        skip_log.is_empty(),
        "copy should succeed on the first run, got: {:?}",
        skip_log.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    let target = dst.path().join("x");
    assert!(target.is_file(), "dst/x should now be a file");
    assert_eq!(fs::read(&target).unwrap(), b"iamafile");

    // No staging file may survive a successful copy.
    let leftovers: Vec<_> = fs::read_dir(dst.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("__dirsync_tmp__"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

/// The pre-clear must never destroy excluded content: RmDir refuses to remove a
/// non-empty directory, so the copy fails loudly instead.
#[tokio::test]
async fn test_excluded_content_blocks_replacement_rather_than_being_deleted() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    fs::write(src.path().join("x"), b"iamafile").unwrap();
    fs::create_dir(dst.path().join("x")).unwrap();
    let precious = dst.path().join("x").join("keep.tmp");
    fs::write(&precious, b"precious").unwrap();

    let config = AppConfig::default().with_extra_excludes(vec!["*.tmp".to_owned()]);
    let engine = SyncEngine::new(
        src.path().to_path_buf(),
        dst.path().to_path_buf(),
        Arc::new(config),
    );
    let plan = engine.preview(None, None).await.unwrap();

    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (_, cancel_rx) = watch::channel(false);
    let skip_log = engine.run(plan, progress, false, pause_rx, cancel_rx).await;

    assert!(!skip_log.is_empty(), "copy should fail rather than clobber");
    assert!(precious.exists(), "excluded file must survive");
    assert_eq!(fs::read(&precious).unwrap(), b"precious");
}
