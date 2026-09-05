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

/// Run a plan to completion with default options and return the skip log.
async fn run_plan(plan: SyncPlan, dry_run: bool) -> dirsync::error::SkipLog {
    run_plan_with(
        plan,
        ExecuteOptions {
            dry_run,
            hdd: false,
        },
    )
    .await
    .0
}

/// Same, but exposes the options and hands back the progress state so a test
/// can assert on what the run reported.
async fn run_plan_with(
    plan: SyncPlan,
    opts: ExecuteOptions,
) -> (
    dirsync::error::SkipLog,
    Arc<dirsync::progress::ProgressState>,
) {
    let (progress, _rx) = new_progress_channel();
    let (_pause_tx, pause_rx) = watch::channel(false);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let skip_log = execute(plan, progress.clone(), opts, pause_rx, cancel_rx).await;
    (skip_log, progress)
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

// --- Move ordering: chains and cycles ---

/// Move ops are handed to the executor in an order that would clobber data if
/// run as given: `a -> b` first would destroy the `b` that `b -> c` still has
/// to read. The topological sort has to reverse them.
#[tokio::test]
async fn test_move_chain_runs_in_dependency_order() {
    let dst = TempDir::new().unwrap();
    fs::write(dst.path().join("a.txt"), b"content-a").unwrap();
    fs::write(dst.path().join("b.txt"), b"content-b").unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![
            SyncOp::Move {
                from: dst.path().join("a.txt"),
                to: dst.path().join("b.txt"),
                is_dir: false,
            },
            SyncOp::Move {
                from: dst.path().join("b.txt"),
                to: dst.path().join("c.txt"),
                is_dir: false,
            },
        ],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty(), "no move should fail");
    assert_eq!(fs::read(dst.path().join("b.txt")).unwrap(), b"content-a");
    assert_eq!(fs::read(dst.path().join("c.txt")).unwrap(), b"content-b");
    assert!(!dst.path().join("a.txt").exists());
}

/// A swap has no safe order at all: the cycle breaker must stage one side
/// through a temp path and clean it up again.
#[tokio::test]
async fn test_move_cycle_is_broken_with_a_temp_rename() {
    let dst = TempDir::new().unwrap();
    fs::write(dst.path().join("a.txt"), b"content-a").unwrap();
    fs::write(dst.path().join("b.txt"), b"content-b").unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![
            SyncOp::Move {
                from: dst.path().join("a.txt"),
                to: dst.path().join("b.txt"),
                is_dir: false,
            },
            SyncOp::Move {
                from: dst.path().join("b.txt"),
                to: dst.path().join("a.txt"),
                is_dir: false,
            },
        ],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty(), "the swap should complete");
    assert_eq!(fs::read(dst.path().join("a.txt")).unwrap(), b"content-b");
    assert_eq!(fs::read(dst.path().join("b.txt")).unwrap(), b"content-a");

    // The swap file is staging litter: leaving one behind would make it a
    // rename-detection candidate on the next run.
    let leftovers: Vec<_> = fs::read_dir(dst.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("__dirsync_swap_"))
        .collect();
    assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
}

/// Three files rotating positions: a longer cycle than the two-file swap, and
/// the case where the chain has to unwind after the breaker fires.
#[tokio::test]
async fn test_three_way_move_cycle() {
    let dst = TempDir::new().unwrap();
    for name in ["a", "b", "c"] {
        fs::write(
            dst.path().join(format!("{name}.txt")),
            format!("content-{name}"),
        )
        .unwrap();
    }

    let rotate = |from: &str, to: &str| SyncOp::Move {
        from: dst.path().join(format!("{from}.txt")),
        to: dst.path().join(format!("{to}.txt")),
        is_dir: false,
    };
    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![rotate("a", "b"), rotate("b", "c"), rotate("c", "a")],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty());
    assert_eq!(fs::read(dst.path().join("b.txt")).unwrap(), b"content-a");
    assert_eq!(fs::read(dst.path().join("c.txt")).unwrap(), b"content-b");
    assert_eq!(fs::read(dst.path().join("a.txt")).unwrap(), b"content-c");
}

/// A single move needs no sorting at all: the early return for n <= 1.
#[tokio::test]
async fn test_single_move_needs_no_sorting() {
    let dst = TempDir::new().unwrap();
    fs::write(dst.path().join("a.txt"), b"payload").unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::Move {
            from: dst.path().join("a.txt"),
            to: dst.path().join("sub/b.txt"),
            is_dir: false,
        }],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty());
    // The move op creates the missing parent directory itself.
    assert_eq!(fs::read(dst.path().join("sub/b.txt")).unwrap(), b"payload");
}

/// A DST file may legally share its name with a new SRC directory. Its Delete
/// has to be hoisted ahead of the directory move, or the rename fails.
#[tokio::test]
async fn test_file_occupying_a_dir_move_target_is_deleted_first() {
    let dst = TempDir::new().unwrap();
    fs::create_dir(dst.path().join("old")).unwrap();
    fs::write(dst.path().join("old/inner.txt"), b"inner").unwrap();
    fs::write(dst.path().join("new"), b"in the way").unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![
            SyncOp::Move {
                from: dst.path().join("old"),
                to: dst.path().join("new"),
                is_dir: true,
            },
            SyncOp::Delete {
                path: dst.path().join("new"),
                size: 10,
            },
        ],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty(), "the blocking delete should be hoisted");
    assert_eq!(
        fs::read(dst.path().join("new/inner.txt")).unwrap(),
        b"inner"
    );
}

/// A MkDir inside a renamed subtree targets the post-rename path, so it must
/// not run in phase 1: doing so would materialize the target and break the
/// directory move.
#[tokio::test]
async fn test_mkdir_inside_a_renamed_subtree_runs_after_the_move() {
    let dst = TempDir::new().unwrap();
    fs::create_dir(dst.path().join("old")).unwrap();
    fs::write(dst.path().join("old/inner.txt"), b"inner").unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![
            SyncOp::MkDir {
                path: dst.path().join("new/fresh"),
            },
            SyncOp::Move {
                from: dst.path().join("old"),
                to: dst.path().join("new"),
                is_dir: true,
            },
        ],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty());
    assert!(dst.path().join("new/inner.txt").exists());
    assert!(dst.path().join("new/fresh").is_dir());
}

// --- Op kinds ---

#[tokio::test]
async fn test_rmdir_keeps_a_directory_that_is_not_empty() {
    let dst = TempDir::new().unwrap();
    fs::create_dir(dst.path().join("keep")).unwrap();
    fs::write(dst.path().join("keep/excluded.txt"), b"user data").unwrap();
    fs::create_dir(dst.path().join("gone")).unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![
            SyncOp::RmDir {
                path: dst.path().join("keep"),
            },
            SyncOp::RmDir {
                path: dst.path().join("gone"),
            },
        ],
    );

    let skip_log = run_plan(plan, false).await;

    // A non-empty directory holds content the plan never touched (excluded
    // files): removing it would destroy data the user kept on purpose.
    assert!(
        skip_log.is_empty(),
        "a full directory is skipped, not failed"
    );
    assert!(dst.path().join("keep/excluded.txt").exists());
    assert!(!dst.path().join("gone").exists());
}

#[tokio::test]
async fn test_delete_op_removes_files_and_directories() {
    let dst = TempDir::new().unwrap();
    fs::write(dst.path().join("file.txt"), b"orphan").unwrap();
    fs::create_dir(dst.path().join("dir")).unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![
            SyncOp::Delete {
                path: dst.path().join("file.txt"),
                size: 6,
            },
            // remove_file fails on a directory, so the op falls back to
            // remove_dir: the same path a directory symlink takes.
            SyncOp::Delete {
                path: dst.path().join("dir"),
                size: 0,
            },
        ],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty());
    assert!(!dst.path().join("file.txt").exists());
    assert!(!dst.path().join("dir").exists());
}

#[tokio::test]
async fn test_mkdir_clears_a_file_sitting_at_the_target() {
    let dst = TempDir::new().unwrap();
    fs::write(dst.path().join("target"), b"in the way").unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::MkDir {
            path: dst.path().join("target"),
        }],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty());
    assert!(dst.path().join("target").is_dir());
}

#[tokio::test]
async fn test_touch_mtime_copies_the_source_timestamp() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let src_file = src.path().join("a.txt");
    let dst_file = dst.path().join("a.txt");
    fs::write(&src_file, b"same").unwrap();
    fs::write(&dst_file, b"same").unwrap();

    // Backdate SRC so the two mtimes cannot coincide by accident.
    let target = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_mtime(&src_file, target).unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::TouchMtime {
            src: src_file.clone(),
            dst: dst_file.clone(),
        }],
    );

    let skip_log = run_plan(plan, false).await;

    assert!(skip_log.is_empty());
    let src_mtime = fs::metadata(&src_file).unwrap().modified().unwrap();
    let dst_mtime = fs::metadata(&dst_file).unwrap().modified().unwrap();
    assert_eq!(src_mtime, dst_mtime);
}

#[tokio::test]
async fn test_symlink_op_creates_a_link_and_replaces_what_is_there() {
    let dst = TempDir::new().unwrap();
    fs::write(dst.path().join("link"), b"a regular file first").unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::Symlink {
            target: std::path::PathBuf::from("target.txt"),
            dst: dst.path().join("link"),
        }],
    );

    let skip_log = run_plan(plan, false).await;

    // Windows needs Developer Mode or elevation to create a symlink; when the
    // syscall is refused the op lands in the skip log instead, which is the
    // documented behaviour rather than a test failure.
    if skip_log.is_empty() {
        let meta = fs::symlink_metadata(dst.path().join("link")).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            fs::read_link(dst.path().join("link")).unwrap(),
            std::path::PathBuf::from("target.txt")
        );
    }
}

// --- Options ---

#[tokio::test]
async fn test_hdd_mode_copies_small_files_serially() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let mut ops = vec![];
    for i in 0..5 {
        let name = format!("f{i}.txt");
        let payload = format!("payload-{i}");
        fs::write(src.path().join(&name), &payload).unwrap();
        ops.push(SyncOp::Copy {
            src: src.path().join(&name),
            dst: dst.path().join(&name),
            size: payload.len() as u64,
            hash: None,
        });
    }

    let (skip_log, progress) = run_plan_with(
        bare_plan(dst.path().to_path_buf(), ops),
        ExecuteOptions {
            dry_run: false,
            hdd: true,
        },
    )
    .await;

    assert!(skip_log.is_empty());
    for i in 0..5 {
        let name = format!("f{i}.txt");
        assert_eq!(
            fs::read(dst.path().join(&name)).unwrap(),
            format!("payload-{i}").into_bytes()
        );
    }
    assert_eq!(*progress.status.read().unwrap(), SyncStatus::Done);
}

#[tokio::test]
async fn test_dry_run_reports_progress_without_writing() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    fs::write(src.path().join("a.txt"), b"payload").unwrap();
    fs::create_dir(dst.path().join("doomed")).unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![
            SyncOp::Copy {
                src: src.path().join("a.txt"),
                dst: dst.path().join("a.txt"),
                size: 7,
                hash: None,
            },
            SyncOp::MkDir {
                path: dst.path().join("fresh"),
            },
            SyncOp::RmDir {
                path: dst.path().join("doomed"),
            },
        ],
    );

    let (skip_log, progress) = run_plan_with(
        plan,
        ExecuteOptions {
            dry_run: true,
            hdd: false,
        },
    )
    .await;

    assert!(skip_log.is_empty());
    assert!(!dst.path().join("a.txt").exists(), "dry run must not write");
    assert!(!dst.path().join("fresh").exists());
    assert!(dst.path().join("doomed").exists());
    // Every op is still counted so the progress bar advances during a dry run.
    assert_eq!(
        progress.ops_done.load(std::sync::atomic::Ordering::Relaxed),
        3
    );
}

#[tokio::test]
async fn test_cancel_during_a_large_copy_leaves_no_staging_file() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let payload = vec![b'x'; 16 * 1024 * 1024];
    fs::write(src.path().join("big.bin"), &payload).unwrap();

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::Copy {
            src: src.path().join("big.bin"),
            dst: dst.path().join("big.bin"),
            size: payload.len() as u64,
            hash: None,
        }],
    );

    let (progress, _rx) = new_progress_channel();
    let (_pause_tx, pause_rx) = watch::channel(false);
    let (cancel_tx, cancel_rx) = watch::channel(false);

    let handle = tokio::spawn(execute(
        plan,
        progress.clone(),
        ExecuteOptions {
            dry_run: false,
            hdd: false,
        },
        pause_rx,
        cancel_rx,
    ));
    tokio::time::sleep(Duration::from_millis(5)).await;
    // A fast machine can finish the copy inside those 5ms, dropping the
    // receiver and making the send fail. That is a valid outcome, not a test
    // failure: the invariant below holds either way.
    let _ = cancel_tx.send(true);
    handle.await.unwrap();

    // Whether the copy beat the cancel or not, the staging file must be gone:
    // one left behind would join the next run's rename detection.
    let leftovers: Vec<_> = fs::read_dir(dst.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("__dirsync_tmp__"))
        .collect();
    assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
}
