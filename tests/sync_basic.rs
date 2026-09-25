use dirsync::config::AppConfig;
use dirsync::progress::new_progress_channel;
use dirsync::sync::SyncEngine;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::watch;

mod common;

/// Execute `plan` and fail the test on any op error: a final tree that
/// happens to look right does not prove every op succeeded.
async fn run_sync(engine: &SyncEngine, plan: dirsync::sync::planner::SyncPlan) {
    let log = run_sync_logged(engine, plan).await;
    let errors: Vec<String> = log
        .iter()
        .map(|e| format!("{}: {}", e.path.display(), e.message))
        .collect();
    assert!(errors.is_empty(), "the run reported op errors: {errors:#?}");
}

/// Execute `plan` and hand back the skip log, for tests that expect errors.
async fn run_sync_logged(
    engine: &SyncEngine,
    plan: dirsync::sync::planner::SyncPlan,
) -> dirsync::error::SkipLog {
    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (_, cancel_rx) = watch::channel(false);
    engine.run(plan, progress, false, pause_rx, cancel_rx).await
}

fn write_file(dir: &std::path::Path, rel: &str, content: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn read_file(dir: &std::path::Path, rel: &str) -> Vec<u8> {
    fs::read(dir.join(rel)).unwrap()
}

fn exists(dir: &std::path::Path, rel: &str) -> bool {
    dir.join(rel).exists()
}

// --- Scenario: unmatchable SRC files get copied ---

#[tokio::test]
async fn test_copy_new_file() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    write_file(src.path(), "hello.txt", b"hello world");

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(plan.copy_count, 1);
    assert_eq!(plan.delete_count, 0);

    run_sync(&engine, plan).await;

    assert_eq!(read_file(dst.path(), "hello.txt"), b"hello world");
}

#[tokio::test]
async fn test_new_src_files_copied() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    // SRC has two files DST doesn't have: both must be copied.
    write_file(src.path(), "a.txt", b"aaa");
    write_file(src.path(), "sub/b.txt", b"bbb");

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(
        plan.copy_count, 2,
        "both new SRC files should be planned for copy"
    );

    run_sync(&engine, plan).await;

    assert_eq!(read_file(dst.path(), "a.txt"), b"aaa");
    assert_eq!(read_file(dst.path(), "sub/b.txt"), b"bbb");
}

// --- Scenario: unmatchable DST files get deleted ---

#[tokio::test]
async fn test_delete_orphan() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    write_file(dst.path(), "orphan.txt", b"delete me");

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(plan.delete_count, 1);

    run_sync(&engine, plan).await;

    assert!(!exists(dst.path(), "orphan.txt"));
}

#[tokio::test]
async fn test_dst_only_files_deleted() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    // DST has files that have no counterpart in SRC and same-size match is impossible
    // (unique content per file, so no false move detection).
    write_file(src.path(), "keep.txt", b"keep");
    write_file(dst.path(), "keep.txt", b"keep");
    write_file(dst.path(), "orphan1.txt", b"orphan-one-111");
    write_file(dst.path(), "sub/orphan2.txt", b"orphan-two-222");

    // Sync mtime on keep.txt so it's Identical.
    let src_meta = fs::metadata(src.path().join("keep.txt")).unwrap();
    let mtime = filetime::FileTime::from_system_time(src_meta.modified().unwrap());
    filetime::set_file_mtime(dst.path().join("keep.txt"), mtime).ok();

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(
        plan.delete_count, 2,
        "both orphan files should be planned for deletion"
    );
    assert_eq!(plan.identical_count, 1);

    run_sync(&engine, plan).await;

    assert!(exists(dst.path(), "keep.txt"));
    assert!(!exists(dst.path(), "orphan1.txt"));
    assert!(!exists(dst.path(), "sub/orphan2.txt"));
}

// --- Scenario: identical files stay (no copy, no delete) ---

#[tokio::test]
async fn test_identical_file_skipped() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    let content = b"same content";
    write_file(src.path(), "same.txt", content);
    write_file(dst.path(), "same.txt", content);

    // Set matching mtime so they look identical without fingerprinting
    let src_path = src.path().join("same.txt");
    let dst_path = dst.path().join("same.txt");
    let src_meta = fs::metadata(&src_path).unwrap();
    let mtime = filetime::FileTime::from_system_time(src_meta.modified().unwrap());
    filetime::set_file_mtime(&dst_path, mtime).ok();

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(plan.copy_count, 0);
    assert_eq!(plan.identical_count, 1);
}

#[tokio::test]
async fn test_identical_files_untouched() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    // Three identical files: none should be copied, moved, or deleted.
    for name in &["a.txt", "b.txt", "sub/c.txt"] {
        let content = format!("content of {name}");
        write_file(src.path(), name, content.as_bytes());
        write_file(dst.path(), name, content.as_bytes());
        let mtime = filetime::FileTime::from_system_time(
            fs::metadata(src.path().join(name))
                .unwrap()
                .modified()
                .unwrap(),
        );
        filetime::set_file_mtime(dst.path().join(name), mtime).ok();
    }

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(plan.identical_count, 3);
    assert_eq!(plan.copy_count, 0);
    assert_eq!(plan.move_count, 0);
    assert_eq!(plan.delete_count, 0);
    assert_eq!(plan.overwrite_count, 0);
}

#[tokio::test]
async fn test_dry_run_no_writes() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "file.txt", b"content");

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (_, cancel_rx) = watch::channel(false);
    engine.run(plan, progress, true, pause_rx, cancel_rx).await;

    assert!(!exists(dst.path(), "file.txt"));
}

#[tokio::test]
async fn test_nested_directory_sync() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    write_file(src.path(), "a/b/deep.txt", b"deep");
    write_file(src.path(), "a/shallow.txt", b"shallow");

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(plan.copy_count, 2);

    run_sync(&engine, plan).await;

    assert_eq!(read_file(dst.path(), "a/b/deep.txt"), b"deep");
    assert_eq!(read_file(dst.path(), "a/shallow.txt"), b"shallow");
}

// --- Scenario: renamed directory gets a single OS-level Move ---

#[tokio::test]
async fn test_renamed_dir_moved() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    // SRC calls the directory "new_name"; DST still has it as "old_name".
    // The three files inside are identical (same relative paths + sizes).
    write_file(src.path(), "new_name/alpha.txt", b"alpha content here");
    write_file(src.path(), "new_name/beta.txt", b"beta content here!");
    write_file(src.path(), "new_name/sub/gamma.txt", b"gamma content!!!!!");

    write_file(dst.path(), "old_name/alpha.txt", b"alpha content here");
    write_file(dst.path(), "old_name/beta.txt", b"beta content here!");
    write_file(dst.path(), "old_name/sub/gamma.txt", b"gamma content!!!!!");

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    // Should be exactly 1 Move (the directory), nothing else.
    assert_eq!(
        plan.move_count, 1,
        "renamed dir should produce exactly one Move op"
    );
    assert_eq!(
        plan.copy_count, 0,
        "no files should be re-copied after dir move"
    );
    assert_eq!(
        plan.delete_count, 0,
        "no files should be deleted after dir move"
    );

    run_sync(&engine, plan).await;

    // After sync: files exist under new_name, old_name is gone.
    assert_eq!(
        read_file(dst.path(), "new_name/alpha.txt"),
        b"alpha content here"
    );
    assert_eq!(
        read_file(dst.path(), "new_name/beta.txt"),
        b"beta content here!"
    );
    assert_eq!(
        read_file(dst.path(), "new_name/sub/gamma.txt"),
        b"gamma content!!!!!"
    );
    assert!(
        !exists(dst.path(), "old_name"),
        "old directory name should no longer exist"
    );
}

// --- Scenario: SRC content the walk could not read ---

#[tokio::test]
async fn unreadable_src_subdir_keeps_its_dst_mirror() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "locked/a.txt", b"aaa");
    write_file(src.path(), "moved.txt", b"payload-bbb");
    write_file(dst.path(), "locked/a.txt", b"aaa");
    write_file(dst.path(), "locked/old.txt", b"old");
    // A DST file under the unreadable dir must not be claimed as a move
    // source either: it may still exist in SRC.
    write_file(dst.path(), "locked/twin.txt", b"payload-bbb");
    let Some(_guard) = common::deny_listing(&src.path().join("locked")) else {
        eprintln!("skipped: cannot deny listing here");
        return;
    };

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(plan.delete_count, 0, "{:?}", plan.ops);
    assert_eq!(plan.move_count, 0, "{:?}", plan.ops);
    assert_eq!(plan.walk_errors.len(), 1, "{:?}", plan.walk_errors);
    run_sync(&engine, plan).await;
    assert!(exists(dst.path(), "locked/old.txt"));
    assert!(exists(dst.path(), "locked/twin.txt"));
    assert_eq!(read_file(dst.path(), "moved.txt"), b"payload-bbb");
}

#[tokio::test]
async fn unreadable_src_root_fails_the_preview() {
    let parent = TempDir::new().unwrap();
    let src = parent.path().join("src");
    let dst = TempDir::new().unwrap();
    write_file(&src, "a.txt", b"aaa");
    write_file(dst.path(), "a.txt", b"aaa");
    write_file(dst.path(), "b.txt", b"bbb");
    let Some(_guard) = common::deny_listing(&src) else {
        eprintln!("skipped: cannot deny listing here");
        return;
    };

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.clone(), dst.path().to_path_buf(), config);

    assert!(engine.preview(None, None).await.is_err());
}

// --- Scenario: a DST file sits where SRC changes type ---

/// Preview and run once, then assert a second preview has nothing left to do.
async fn sync_and_assert_converged(src: &std::path::Path, dst: &std::path::Path) {
    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.to_path_buf(), dst.to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();
    run_sync(&engine, plan).await;
    let again = engine.preview(None, None).await.unwrap();
    assert!(again.is_noop(), "second run still plans: {:?}", again.ops);
}

#[tokio::test]
async fn a_dst_file_where_src_has_a_directory_is_not_used_as_a_move_source() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "x/inner.txt", b"inner");
    write_file(src.path(), "y", b"payload-content");
    write_file(dst.path(), "x", b"payload-content");

    sync_and_assert_converged(src.path(), dst.path()).await;

    // MkDir x used to delete the move source, then `Move x -> y` renamed the
    // new empty directory to y.
    assert_eq!(read_file(dst.path(), "y"), b"payload-content");
    assert_eq!(read_file(dst.path(), "x/inner.txt"), b"inner");
}

#[tokio::test]
async fn a_file_replacing_its_own_parent_directory_converges() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "P", b"DDDD");
    write_file(dst.path(), "P/inner", b"DDDD");

    sync_and_assert_converged(src.path(), dst.path()).await;

    assert_eq!(read_file(dst.path(), "P"), b"DDDD");
}

#[tokio::test]
async fn a_dst_file_under_a_directory_src_replaces_with_a_file_is_not_a_move_source() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "X", b"new-file");
    write_file(src.path(), "Y", b"AAAA");
    write_file(dst.path(), "X/a", b"AAAA");

    sync_and_assert_converged(src.path(), dst.path()).await;

    assert_eq!(read_file(dst.path(), "X"), b"new-file");
    assert_eq!(read_file(dst.path(), "Y"), b"AAAA");
}

#[tokio::test]
async fn excluded_content_blocking_a_file_reports_what_blocks_it() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "X", b"file now");
    write_file(dst.path(), "X/keep.tmp", b"excluded, never deleted");

    let config = Arc::new(AppConfig {
        exclude_patterns: vec!["*.tmp".into()],
        ..AppConfig::default()
    });
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();
    let log = run_sync_logged(&engine, plan).await;

    // The excluded file survives, and the error names it instead of a bare
    // "Access is denied" from the rename that could not replace the dir.
    assert!(exists(dst.path(), "X/keep.tmp"));
    let messages: Vec<String> = log.iter().map(|e| e.message.clone()).collect();
    assert!(
        messages.iter().any(|m| m.contains("keep.tmp")),
        "{messages:?}"
    );
}

// --- Scenario: names that differ only in letter case ---

#[tokio::test]
async fn a_case_rename_with_an_edit_keeps_the_file() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    // Sizes differ: same size within the 3 s mtime tolerance is Identical
    // by design (see decisions.md, MTIME_TOLERANCE).
    write_file(src.path(), "photo.jpg", b"edited content");
    write_file(dst.path(), "Photo.jpg", b"original");

    sync_and_assert_converged(src.path(), dst.path()).await;

    assert_eq!(read_file(dst.path(), "photo.jpg"), b"edited content");
    let names: Vec<String> = fs::read_dir(dst.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["photo.jpg"]);
}

#[tokio::test]
async fn a_new_dir_replacing_a_file_that_differs_only_in_case_survives() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    fs::create_dir(src.path().join("a")).unwrap();
    write_file(dst.path(), "A", b"file");

    sync_and_assert_converged(src.path(), dst.path()).await;

    assert!(dst.path().join("a").is_dir());
}

// --- Scenario: Windows junctions ---

#[cfg(windows)]
#[tokio::test]
async fn a_src_junction_is_mirrored_as_a_junction_without_privileges() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    write_file(target.path(), "inside.txt", b"through the junction");
    // Junctions need no SeCreateSymbolicLinkPrivilege, unlike symlinks.
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(src.path().join("j"))
        .arg(target.path())
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");

    let config = Arc::new(AppConfig::default());
    let engine = SyncEngine::new(src.path().to_path_buf(), dst.path().to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();
    run_sync(&engine, plan).await;
    assert_eq!(
        fs::read(dst.path().join("j").join("inside.txt")).unwrap(),
        b"through the junction"
    );
    let again = engine.preview(None, None).await.unwrap();
    assert!(again.is_noop(), "{:?}", again.ops);
}

// --- Scenario: moves and renames converge in one run ---

fn set_mtime(dir: &std::path::Path, rel: &str, secs_ago: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
    filetime::set_file_mtime(dir.join(rel), filetime::FileTime::from_system_time(t)).unwrap();
}

#[tokio::test]
async fn a_moved_file_gets_its_mtime_fixed_in_the_same_run() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "new/place.bin", b"moved payload");
    // A sibling makes the directories differ, so this is a file-level move
    // rather than a directory rename.
    write_file(src.path(), "new/sibling.txt", b"s");
    write_file(dst.path(), "old/place.bin", b"moved payload");
    set_mtime(src.path(), "new/place.bin", 100);
    set_mtime(dst.path(), "old/place.bin", 5000);

    // Used to take three runs: move, then touch, then nothing.
    sync_and_assert_converged(src.path(), dst.path()).await;
}

#[tokio::test]
async fn a_false_directory_rename_pairing_still_syncs_content() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    // Same relative paths and sizes: the directories look like a rename.
    write_file(src.path(), "new/a.txt", b"BBBB");
    write_file(dst.path(), "old/a.txt", b"AAAA");
    set_mtime(src.path(), "new/a.txt", 100);
    set_mtime(dst.path(), "old/a.txt", 102);

    sync_and_assert_converged(src.path(), dst.path()).await;

    // Mtimes within tolerance used to make the pair Identical by the fast
    // path, keeping AAAA under the new name for good.
    assert_eq!(read_file(dst.path(), "new/a.txt"), b"BBBB");
}
