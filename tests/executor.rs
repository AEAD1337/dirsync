use dirsync::config::AppConfig;
use dirsync::progress::{ProgressEvent, SyncStatus, new_progress_channel};
use dirsync::sync::SyncEngine;
use dirsync::sync::executor::{ExecuteOptions, execute};
use dirsync::sync::planner::{LinkKind, SyncOp, SyncPlan};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
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
        dir_blocked_targets: vec![],
        walk_errors: vec![],
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

mod common;

/// What a DST entry looks like on disk, as far as any op could change it.
#[derive(Debug, PartialEq, Eq)]
enum Node {
    File { bytes: Vec<u8>, mtime: SystemTime },
    Dir { mtime: SystemTime },
    Link { target: PathBuf },
}

/// Every entry below `root` keyed by its slash-separated relative path, with
/// names as stored on disk (so a case-only rename shows up), file bytes, and
/// modification times. Symlinks are recorded, never followed.
fn snapshot(root: &Path) -> BTreeMap<String, Node> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, Node>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let meta = fs::symlink_metadata(&path).unwrap();
            let node = if meta.file_type().is_symlink() {
                Node::Link {
                    target: fs::read_link(&path).unwrap(),
                }
            } else if meta.is_dir() {
                visit(root, &path, out);
                Node::Dir {
                    mtime: meta.modified().unwrap(),
                }
            } else {
                Node::File {
                    bytes: fs::read(&path).unwrap(),
                    mtime: meta.modified().unwrap(),
                }
            };
            out.insert(rel, node);
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

/// Relative paths below `root` that are dirsync staging litter (copy temp
/// files, swap files, case-rename intermediates).
fn staging_litter(root: &Path) -> Vec<String> {
    snapshot(root)
        .into_keys()
        .filter(|p| p.contains("__dirsync_"))
        .collect()
}

fn skip_messages(log: &dirsync::error::SkipLog) -> Vec<String> {
    log.iter()
        .map(|e| format!("{}: {}", e.path.display(), e.message))
        .collect()
}

/// Wait (bounded) for the first event `pred` accepts.
async fn wait_for_event(
    rx: &mut tokio::sync::broadcast::Receiver<ProgressEvent>,
    pred: impl Fn(&ProgressEvent) -> bool,
) {
    let found = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Ok(ev) if pred(&ev) => return true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await;
    assert_eq!(found, Ok(true), "the expected progress event never arrived");
}

/// Await a spawned run, failing instead of hanging if it never ends.
async fn join_run(
    handle: tokio::task::JoinHandle<dirsync::error::SkipLog>,
) -> dirsync::error::SkipLog {
    tokio::time::timeout(Duration::from_secs(20), handle)
        .await
        .expect("the run did not end after the cancel")
        .unwrap()
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
    assert!(
        snapshot(dst.path()).is_empty(),
        "no copy may run after a cancel: {:?}",
        snapshot(dst.path()).keys().collect::<Vec<_>>()
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

    assert!(skip_log.is_empty(), "{:?}", skip_messages(&skip_log));
    assert_eq!(fs::read(dst.path().join("b.txt")).unwrap(), b"content-a");
    assert_eq!(fs::read(dst.path().join("c.txt")).unwrap(), b"content-b");
    assert_eq!(fs::read(dst.path().join("a.txt")).unwrap(), b"content-c");
    assert!(staging_litter(dst.path()).is_empty());
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
            kind: dirsync::sync::planner::LinkKind::File,
        }],
    );

    let skip_log = run_plan(plan, false).await;

    let errors: Vec<String> = skip_log.iter().map(|e| e.message.clone()).collect();
    if errors.is_empty() {
        let meta = fs::symlink_metadata(dst.path().join("link")).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            fs::read_link(dst.path().join("link")).unwrap(),
            std::path::PathBuf::from("target.txt")
        );
    } else {
        // Windows needs Developer Mode or elevation to create a symlink: the
        // refused syscall lands in the skip log, and it must be exactly that
        // refusal, not some other failure of the op.
        assert_eq!(errors.len(), 1, "{errors:?}");
        common::assert_privilege_error(&errors[0], None);
    }
}

// --- Options ---

/// With hdd: true, small files go through the serial scheduler: each copy
/// starts, finishes and is counted before the next one starts, in plan
/// order. The parallel small-file path emits no FileStarted/FileDone at all
/// (and a parallel scheduler would interleave them), so this sequence only
/// comes out of a serial run.
#[tokio::test]
async fn test_hdd_mode_copies_small_files_one_at_a_time() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let mut ops = vec![];
    let mut expected = vec![];
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
        expected.extend([
            format!("start {name}"),
            format!("done {name}"),
            format!("op {name}"),
        ]);
    }

    let (progress, mut rx) = new_progress_channel();
    let (_pause_tx, pause_rx) = watch::channel(false);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let skip_log = execute(
        bare_plan(dst.path().to_path_buf(), ops),
        progress.clone(),
        ExecuteOptions {
            dry_run: false,
            hdd: true,
        },
        pause_rx,
        cancel_rx,
    )
    .await;

    assert!(skip_log.is_empty(), "{:?}", skip_messages(&skip_log));
    let mut seen = vec![];
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ProgressEvent::FileStarted { name, .. } => seen.push(format!("start {name}")),
            ProgressEvent::FileDone { name } => seen.push(format!("done {name}")),
            ProgressEvent::OpDone { path, .. } => {
                let name = Path::new(&path).file_name().unwrap().to_string_lossy();
                seen.push(format!("op {name}"));
            }
            _ => {}
        }
    }
    assert_eq!(seen, expected, "copies overlapped or ran out of order");
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

// --- Cancel mid-run ---

/// A plan with a small and a large copy and three orphans. Returns the plan
/// and the orphan paths.
fn copy_and_orphan_plan(src: &Path, dst: &Path) -> (SyncPlan, Vec<PathBuf>) {
    let mut ops = vec![];
    for (name, size) in [("small.txt", 16usize), ("large.bin", 1024 * 1024 + 7)] {
        fs::write(src.join(name), vec![b'x'; size]).unwrap();
        ops.push(SyncOp::Copy {
            src: src.join(name),
            dst: dst.join(name),
            size: size as u64,
            hash: None,
        });
    }
    let mut orphans = vec![];
    for i in 0..3 {
        let orphan = dst.join(format!("orphan{i}.txt"));
        fs::write(&orphan, b"still here").unwrap();
        ops.push(SyncOp::Delete {
            path: orphan.clone(),
            size: 10,
        });
        orphans.push(orphan);
    }
    (bare_plan(dst.to_path_buf(), ops), orphans)
}

/// Start `plan` paused, wait until the executor reports Paused, cancel, and
/// return the skip log and progress state of the finished run.
async fn cancel_while_paused(
    plan: SyncPlan,
    hdd: bool,
) -> (
    dirsync::error::SkipLog,
    Arc<dirsync::progress::ProgressState>,
) {
    let (progress, mut rx) = new_progress_channel();
    let (_pause_tx, pause_rx) = watch::channel(true);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handle = tokio::spawn(execute(
        plan,
        progress.clone(),
        ExecuteOptions {
            dry_run: false,
            hdd,
        },
        pause_rx,
        cancel_rx,
    ));

    wait_for_event(&mut rx, |ev| {
        matches!(
            ev,
            ProgressEvent::StatusChanged {
                status: SyncStatus::Paused
            }
        )
    })
    .await;
    assert_eq!(*progress.status.read().unwrap(), SyncStatus::Paused);
    cancel_tx.send(true).unwrap();
    let log = join_run(handle).await;
    (log, progress)
}

/// Cancelling a paused run ends it as Cancelled without running the op it
/// was parked in front of. Paused before the Delete phase, a cancel must
/// delete nothing: every orphan survives and DST is unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_while_paused_runs_no_further_op() {
    let dst = TempDir::new().unwrap();
    let orphans: Vec<PathBuf> = (0..3)
        .map(|i| dst.path().join(format!("orphan{i}.txt")))
        .collect();
    let mut ops = vec![];
    for o in &orphans {
        fs::write(o, b"still here").unwrap();
        ops.push(SyncOp::Delete {
            path: o.clone(),
            size: 10,
        });
    }
    let before = snapshot(dst.path());

    let (log, progress) =
        cancel_while_paused(bare_plan(dst.path().to_path_buf(), ops), false).await;

    assert_eq!(*progress.status.read().unwrap(), SyncStatus::Cancelled);
    assert!(log.is_empty(), "{:?}", skip_messages(&log));
    let survivors: Vec<_> = orphans.iter().filter(|o| o.exists()).collect();
    assert_eq!(survivors.len(), 3, "a cancel while paused deleted a file");
    assert_eq!(snapshot(dst.path()), before, "a cancelled run changed DST");
    assert_eq!(
        progress.ops_done.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

/// A cancel that lands while the run is parked in the copy phase stops the
/// run there: no copy is started and the Delete phase never runs, so the
/// orphans survive. Both copy schedulers are covered: the parallel
/// small-file loop (hdd: false) and the serial one (hdd: true).
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_while_paused_in_the_copy_phase_skips_the_delete_phase() {
    for hdd in [false, true] {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let (plan, orphans) = copy_and_orphan_plan(src.path(), dst.path());

        let (log, progress) = cancel_while_paused(plan, hdd).await;

        assert_eq!(
            *progress.status.read().unwrap(),
            SyncStatus::Cancelled,
            "hdd={hdd}"
        );
        assert!(log.is_empty(), "hdd={hdd}: {:?}", skip_messages(&log));
        assert!(
            !dst.path().join("small.txt").exists() && !dst.path().join("large.bin").exists(),
            "hdd={hdd}: no copy may start after the cancel"
        );
        assert!(
            orphans.iter().all(|o| o.exists()),
            "hdd={hdd}: the Delete phase ran after a cancel"
        );
        assert!(staging_litter(dst.path()).is_empty(), "hdd={hdd}");
    }
}

/// A cancel that arrives while a large file is being copied interrupts that
/// copy (no staging file, no partial target) and stops every later phase:
/// the orphans queued for the Delete phase survive.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_a_large_copy_stops_the_delete_phase() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let size = 32 * 1024 * 1024;
    fs::write(src.path().join("big.bin"), vec![3u8; size]).unwrap();
    let mut ops = vec![SyncOp::Copy {
        src: src.path().join("big.bin"),
        dst: dst.path().join("big.bin"),
        size: size as u64,
        hash: None,
    }];
    let orphans: Vec<PathBuf> = (0..3)
        .map(|i| dst.path().join(format!("orphan{i}.txt")))
        .collect();
    for o in &orphans {
        fs::write(o, b"still here").unwrap();
        ops.push(SyncOp::Delete {
            path: o.clone(),
            size: 10,
        });
    }

    let (progress, mut rx) = new_progress_channel();
    let (_pause_tx, pause_rx) = watch::channel(false);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handle = tokio::spawn(execute(
        bare_plan(dst.path().to_path_buf(), ops),
        progress.clone(),
        ExecuteOptions {
            dry_run: false,
            hdd: false,
        },
        pause_rx,
        cancel_rx,
    ));
    // The chunked copy polls the token per 256 KB chunk: cancelling on its
    // FileStarted lands inside the file.
    wait_for_event(
        &mut rx,
        |ev| matches!(ev, ProgressEvent::FileStarted { name, .. } if name == "big.bin"),
    )
    .await;
    cancel_tx.send(true).unwrap();
    let log = join_run(handle).await;

    assert_eq!(*progress.status.read().unwrap(), SyncStatus::Cancelled);
    assert!(
        log.is_empty(),
        "a cancel is not a file error: {:?}",
        skip_messages(&log)
    );
    assert!(
        orphans.iter().all(|o| o.exists()),
        "the Delete phase ran after a cancel"
    );
    assert!(
        !dst.path().join("big.bin").exists(),
        "the copy was not interrupted"
    );
    assert!(staging_litter(dst.path()).is_empty());
}

// --- Readonly targets and metadata policy ---

fn set_readonly(path: &std::path::Path, readonly: bool) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms).unwrap();
}

fn is_readonly(path: &std::path::Path) -> bool {
    fs::metadata(path).unwrap().permissions().readonly()
}

/// Content sizes on both sides of the small/large copy threshold.
const SIZES: [usize; 2] = [16, 1024 * 1024 + 7];

#[tokio::test]
async fn overwrite_replaces_a_readonly_dst_file() {
    for size in SIZES {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let s = src.path().join("a.bin");
        let d = dst.path().join("a.bin");
        fs::write(&s, vec![1u8; size]).unwrap();
        fs::write(&d, vec![2u8; size]).unwrap();
        // Readonly in DST because an earlier run mirrored a readonly SRC.
        set_readonly(&d, true);

        let plan = bare_plan(
            dst.path().to_path_buf(),
            vec![SyncOp::Overwrite {
                src: s.clone(),
                dst: d.clone(),
                size: size as u64,
                hash: None,
            }],
        );
        let log = run_plan(plan, false).await;

        let errors: Vec<_> = log.iter().map(|e| e.message.clone()).collect();
        assert!(errors.is_empty(), "size {size}: {errors:?}");
        assert_eq!(fs::read(&d).unwrap(), vec![1u8; size], "size {size}");
        set_readonly(&d, false);
    }
}

#[tokio::test]
async fn touch_mtime_updates_a_readonly_dst_file_and_keeps_it_readonly() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let s = src.path().join("a.txt");
    let d = dst.path().join("a.txt");
    fs::write(&s, b"same").unwrap();
    fs::write(&d, b"same").unwrap();
    let past = std::time::SystemTime::now() - Duration::from_secs(3600);
    filetime::set_file_mtime(&s, filetime::FileTime::from_system_time(past)).unwrap();
    set_readonly(&d, true);

    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::TouchMtime {
            src: s.clone(),
            dst: d.clone(),
        }],
    );
    let log = run_plan(plan, false).await;

    let errors: Vec<_> = log.iter().map(|e| e.message.clone()).collect();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        fs::metadata(&d).unwrap().modified().unwrap(),
        fs::metadata(&s).unwrap().modified().unwrap()
    );
    assert!(is_readonly(&d));
    set_readonly(&d, false);
}

#[tokio::test]
async fn small_and_large_copies_mirror_the_readonly_attribute_alike() {
    for size in SIZES {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let s = src.path().join("a.bin");
        let d = dst.path().join("a.bin");
        fs::write(&s, vec![1u8; size]).unwrap();
        set_readonly(&s, true);

        let plan = bare_plan(
            dst.path().to_path_buf(),
            vec![SyncOp::Copy {
                src: s.clone(),
                dst: d.clone(),
                size: size as u64,
                hash: None,
            }],
        );
        let log = run_plan(plan, false).await;

        assert!(log.is_empty(), "size {size}");
        assert!(is_readonly(&d), "size {size}: readonly must be mirrored");
        assert_eq!(
            fs::metadata(&d).unwrap().modified().unwrap(),
            fs::metadata(&s).unwrap().modified().unwrap(),
            "size {size}"
        );
        set_readonly(&s, false);
        set_readonly(&d, false);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn large_copies_keep_the_exec_bit_like_small_ones() {
    use std::os::unix::fs::PermissionsExt;
    for size in SIZES {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let s = src.path().join("run.sh");
        let d = dst.path().join("run.sh");
        fs::write(&s, vec![b'#'; size]).unwrap();
        fs::set_permissions(&s, fs::Permissions::from_mode(0o750)).unwrap();

        let plan = bare_plan(
            dst.path().to_path_buf(),
            vec![SyncOp::Copy {
                src: s.clone(),
                dst: d.clone(),
                size: size as u64,
                hash: None,
            }],
        );
        assert!(run_plan(plan, false).await.is_empty());

        let mode = fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750, "size {size}");
    }
}

// --- Cancel inside the last op ---

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_the_final_copy_ends_cancelled_not_done_or_failed() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let s = src.path().join("big.bin");
    let d = dst.path().join("big.bin");
    fs::write(&s, vec![7u8; 64 * 1024 * 1024]).unwrap();
    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::Copy {
            src: s.clone(),
            dst: d.clone(),
            size: 64 * 1024 * 1024,
            hash: None,
        }],
    );

    let (progress, mut rx) = new_progress_channel();
    let (_pause_tx, pause_rx) = watch::channel(false);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    // Cancel the moment the copy starts: the chunk loop sees it mid-file.
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if matches!(ev, dirsync::progress::ProgressEvent::FileStarted { .. }) {
                cancel_tx.send_replace(true);
                break;
            }
        }
        // Keep the sender alive until the run is over.
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let opts = ExecuteOptions {
        dry_run: false,
        hdd: false,
    };
    let log = execute(plan, progress.clone(), opts, pause_rx, cancel_rx).await;

    assert_eq!(*progress.status.read().unwrap(), SyncStatus::Cancelled);
    let errors: Vec<_> = log.iter().map(|e| e.message.clone()).collect();
    assert!(
        errors.is_empty(),
        "a cancel is not a file error: {errors:?}"
    );
    assert!(!d.exists());
    assert_eq!(
        fs::read_dir(dst.path()).unwrap().count(),
        0,
        "staging file left"
    );
}

// --- Safety gate covers both ends of a rename ---

#[tokio::test]
async fn the_safety_gate_rejects_a_move_whose_source_is_outside_dst() {
    let dst = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let victim = elsewhere.path().join("victim.txt");
    fs::write(&victim, b"keep me").unwrap();

    // A rename's source disappears from where it was: just as destructive
    // as its target, so it must be inside dst_root too.
    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![SyncOp::Move {
            from: victim.clone(),
            to: dst.path().join("stolen.txt"),
            is_dir: false,
        }],
    );
    let log = run_plan(plan, false).await;

    assert!(!log.is_empty());
    assert!(victim.exists());
}

#[tokio::test]
async fn the_safety_gate_rejects_a_parent_dir_component() {
    let dst = TempDir::new().unwrap();
    let inner = dst.path().join("inner");
    fs::create_dir(&inner).unwrap();
    let src_file = dst.path().join("src.txt");
    fs::write(&src_file, b"x").unwrap();

    // Textually under dst_root, physically beside it.
    let plan = bare_plan(
        inner.clone(),
        vec![SyncOp::Copy {
            src: src_file,
            dst: inner.join("..").join("escaped.txt"),
            size: 1,
            hash: None,
        }],
    );
    let log = run_plan(plan, false).await;

    assert!(!log.is_empty());
    assert!(!dst.path().join("escaped.txt").exists());
}

// --- Dry run ---

/// Content just above the 1 MB small-file threshold, so the op takes the
/// chunked copy path; `seed` makes two such files differ.
fn large_payload(seed: u8) -> Vec<u8> {
    (0..1024 * 1024 + 17)
        .map(|i: usize| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A dry run of a plan holding every op kind must leave DST exactly as it
/// was: same entries (with their stored case), same bytes, same mtimes.
/// Every op arm guards its write with `if !dry_run`; dropping any one of
/// those guards changes this tree. Both copy schedulers are covered, and a
/// write target blocked by a DST directory exercises the phase-1.5 path.
#[tokio::test]
async fn a_dry_run_of_every_op_kind_leaves_dst_unchanged() {
    for hdd in [false, true] {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let (s, d) = (src.path(), dst.path());

        fs::write(s.join("new_small.txt"), b"new small file").unwrap();
        fs::write(s.join("new_large.bin"), large_payload(1)).unwrap();
        fs::write(s.join("ow_small.txt"), b"SRC version").unwrap();
        fs::write(s.join("ow_large.bin"), large_payload(2)).unwrap();
        fs::write(s.join("touch.txt"), b"same").unwrap();
        filetime::set_file_mtime(
            s.join("touch.txt"),
            filetime::FileTime::from_unix_time(1_000_000_000, 0),
        )
        .unwrap();
        fs::write(s.join("blocker"), b"a file where DST has a directory").unwrap();

        fs::write(d.join("ow_small.txt"), b"DST version").unwrap();
        fs::write(d.join("ow_large.bin"), large_payload(3)).unwrap();
        fs::write(d.join("touch.txt"), b"same").unwrap();
        fs::write(d.join("move_me.txt"), b"a file to relocate").unwrap();
        fs::create_dir(d.join("old_dir")).unwrap();
        fs::write(d.join("old_dir/inner.txt"), b"inside a renamed dir").unwrap();
        fs::create_dir(d.join("empty_dir")).unwrap();
        fs::write(d.join("orphan.txt"), b"an orphan").unwrap();
        fs::write(d.join("Case.txt"), b"case only rename").unwrap();
        fs::write(d.join("link_slot"), b"occupies the symlink target").unwrap();
        fs::create_dir(d.join("blocker")).unwrap();
        fs::write(d.join("blocker/inner.txt"), b"inside the blocker").unwrap();

        let copy = |name: &str, size: u64| SyncOp::Copy {
            src: s.join(name),
            dst: d.join(name),
            size,
            hash: None,
        };
        let overwrite = |name: &str, size: u64| SyncOp::Overwrite {
            src: s.join(name),
            dst: d.join(name),
            size,
            hash: None,
        };
        let large = large_payload(0).len() as u64;
        let ops = vec![
            SyncOp::MkDir {
                path: d.join("new_dir"),
            },
            // Inside the dir-move target: runs after the move (phase 2.1).
            SyncOp::MkDir {
                path: d.join("renamed_dir/fresh"),
            },
            SyncOp::Move {
                from: d.join("move_me.txt"),
                to: d.join("moved/move_me.txt"),
                is_dir: false,
            },
            SyncOp::Move {
                from: d.join("old_dir"),
                to: d.join("renamed_dir"),
                is_dir: true,
            },
            SyncOp::CaseRename {
                from: d.join("Case.txt"),
                to: d.join("case.txt"),
                is_dir: false,
            },
            SyncOp::Symlink {
                target: PathBuf::from("target.txt"),
                dst: d.join("link_slot"),
                kind: LinkKind::File,
            },
            copy("new_small.txt", 14),
            copy("new_large.bin", large),
            overwrite("ow_small.txt", 11),
            overwrite("ow_large.bin", large),
            SyncOp::TouchMtime {
                src: s.join("touch.txt"),
                dst: d.join("touch.txt"),
            },
            SyncOp::Delete {
                path: d.join("orphan.txt"),
                size: 9,
            },
            SyncOp::RmDir {
                path: d.join("empty_dir"),
            },
            SyncOp::Delete {
                path: d.join("blocker/inner.txt"),
                size: 18,
            },
            SyncOp::RmDir {
                path: d.join("blocker"),
            },
            copy("blocker", 32),
        ];
        let op_count = ops.len();
        let mut plan = bare_plan(d.to_path_buf(), ops);
        plan.dir_blocked_targets = vec![d.join("blocker")];

        let before = snapshot(d);
        let (log, progress) = run_plan_with(plan, ExecuteOptions { dry_run: true, hdd }).await;

        assert!(log.is_empty(), "hdd={hdd}: {:?}", skip_messages(&log));
        assert_eq!(*progress.status.read().unwrap(), SyncStatus::Done);
        assert_eq!(
            progress.ops_done.load(std::sync::atomic::Ordering::Relaxed),
            op_count,
            "hdd={hdd}: every op is still counted in a dry run"
        );
        assert_eq!(snapshot(d), before, "hdd={hdd}: a dry run changed DST");
    }
}

// --- A failed overwrite ---

/// An Overwrite whose SRC is gone by the time the run reaches it (deleted,
/// or replaced by a directory, after the preview) fails that one op and
/// keeps the old DST content: the copy is staged and only renamed over the
/// target once complete, so a failure can never truncate or remove the file
/// it was meant to replace, nor leave a staging file behind.
#[tokio::test]
async fn a_failed_overwrite_keeps_the_old_dst_content() {
    for size in SIZES {
        for replace_with_dir in [false, true] {
            let case = format!("size {size}, replaced by a dir: {replace_with_dir}");
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            let s = src.path().join("a.bin");
            let d = dst.path().join("a.bin");
            fs::write(&s, vec![1u8; size]).unwrap();
            fs::write(&d, vec![2u8; size]).unwrap();
            // Same size: an old DST mtime forces the content comparison.
            let past = std::time::SystemTime::now() - Duration::from_secs(3600);
            filetime::set_file_mtime(&d, filetime::FileTime::from_system_time(past)).unwrap();

            let engine = SyncEngine::new(
                src.path().to_path_buf(),
                dst.path().to_path_buf(),
                Arc::new(AppConfig::default()),
            );
            let plan = engine.preview(None, None).await.unwrap();
            assert_eq!(plan.overwrite_count, 1, "{case}: {:?}", plan.ops);

            // SRC changes between preview and run.
            fs::remove_file(&s).unwrap();
            if replace_with_dir {
                fs::create_dir(&s).unwrap();
            }
            let (progress, _rx) = new_progress_channel();
            let (_, pause_rx) = watch::channel(false);
            let (_, cancel_rx) = watch::channel(false);
            let log = engine
                .run(plan, progress.clone(), false, pause_rx, cancel_rx)
                .await;

            let entries: Vec<_> = log.iter().collect();
            assert_eq!(entries.len(), 1, "{case}: {:?}", skip_messages(&log));
            assert_eq!(entries[0].path, d, "{case}");
            assert_eq!(fs::read(&d).unwrap(), vec![2u8; size], "{case}");
            assert!(staging_litter(dst.path()).is_empty(), "{case}");
            // One failed file does not stop the run.
            assert_eq!(*progress.status.read().unwrap(), SyncStatus::Done);
        }
    }
}

/// Windows: a SRC file someone holds an exclusive lock on can be opened but
/// not read, so the copy fails only *after* its staging file was created.
/// That failure must still keep the old DST content and remove the staging
/// file (a vanished SRC fails before staging starts, so the test above
/// cannot see the cleanup on Windows).
#[cfg(windows)]
#[tokio::test]
async fn a_failed_overwrite_after_staging_started_leaves_no_staging_file() {
    for size in SIZES {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let s = src.path().join("a.bin");
        let d = dst.path().join("a.bin");
        fs::write(&s, vec![1u8; size]).unwrap();
        fs::write(&d, vec![2u8; size]).unwrap();
        let plan = bare_plan(
            dst.path().to_path_buf(),
            vec![SyncOp::Overwrite {
                src: s.clone(),
                dst: d.clone(),
                size: size as u64,
                hash: None,
            }],
        );

        let holder = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&s)
            .unwrap();
        holder.lock().unwrap();
        let log = run_plan(plan, false).await;
        holder.unlock().unwrap();
        drop(holder);

        let errors = skip_messages(&log);
        assert_eq!(errors.len(), 1, "size {size}: {errors:?}");
        assert_eq!(fs::read(&d).unwrap(), vec![2u8; size], "size {size}");
        assert!(
            staging_litter(dst.path()).is_empty(),
            "size {size}: {:?}",
            staging_litter(dst.path())
        );
    }
}

// --- Move ordering across several independent cycles ---

/// Two disjoint cycles and a chain in one plan, handed over interleaved: the
/// sort has to break each cycle separately (its cursor must move past the
/// first one) and still order the chain.
#[tokio::test]
async fn test_disjoint_move_cycles_and_a_chain_in_one_plan() {
    let dst = TempDir::new().unwrap();
    for name in ["a", "b", "c", "d", "e", "f", "g"] {
        fs::write(
            dst.path().join(format!("{name}.txt")),
            format!("content-{name}"),
        )
        .unwrap();
    }
    let mv = |from: &str, to: &str| SyncOp::Move {
        from: dst.path().join(format!("{from}.txt")),
        to: dst.path().join(format!("{to}.txt")),
        is_dir: false,
    };
    let plan = bare_plan(
        dst.path().to_path_buf(),
        vec![
            mv("f", "g"), // chain f -> g -> h
            mv("a", "b"), // swap a <-> b
            mv("c", "d"), // rotation c -> d -> e -> c
            mv("b", "a"),
            mv("g", "h"),
            mv("d", "e"),
            mv("e", "c"),
        ],
    );

    let log = run_plan(plan, false).await;

    assert!(log.is_empty(), "{:?}", skip_messages(&log));
    let read = |n: &str| fs::read_to_string(dst.path().join(format!("{n}.txt"))).unwrap();
    assert_eq!(read("a"), "content-b");
    assert_eq!(read("b"), "content-a");
    assert_eq!(read("d"), "content-c");
    assert_eq!(read("e"), "content-d");
    assert_eq!(read("c"), "content-e");
    assert_eq!(read("g"), "content-f");
    assert_eq!(read("h"), "content-g");
    assert!(!dst.path().join("f.txt").exists());
    assert!(staging_litter(dst.path()).is_empty());
}
