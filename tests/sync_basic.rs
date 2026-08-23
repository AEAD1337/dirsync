use dirsync::config::AppConfig;
use dirsync::progress::new_progress_channel;
use dirsync::sync::SyncEngine;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::watch;

async fn run_sync(engine: &SyncEngine, plan: dirsync::sync::planner::SyncPlan) {
    let (progress, _rx) = new_progress_channel();
    let (_, pause_rx) = watch::channel(false);
    let (_, cancel_rx) = watch::channel(false);
    engine.run(plan, progress, false, pause_rx, cancel_rx).await;
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
