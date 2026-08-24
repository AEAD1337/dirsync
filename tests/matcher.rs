use dirsync::progress::{LogLevel, ProgressEvent, new_progress_channel};
use dirsync::sync::{matcher::match_trees, walker::FileEntry};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

/// Build a FileEntry pointing to a real file created by the caller.
fn entry(rel: &str, abs_path: PathBuf, size: u64, mtime: SystemTime) -> FileEntry {
    FileEntry {
        rel_path: PathBuf::from(rel),
        abs_path,
        size,
        mtime,
        is_dir: false,
        symlink_target: None,
    }
}

/// 2.4: When hash_file fails (abs_path does not exist), a Warning must be emitted
/// via the progress channel so the user knows why sync behaviour is degraded.
#[test]
fn test_hash_failure_emits_warning() {
    let dir = TempDir::new().unwrap();
    let real_path = dir.path().join("file.bin");
    fs::write(&real_path, b"hello world").unwrap();

    let now = SystemTime::now();
    let old = now - Duration::from_secs(100);

    // src: a real file that can be hashed.
    let src = entry("file.bin", real_path, 11, now);
    // dst: same rel_path and size but different mtime (forces hashing) and a
    // non-existent abs_path so hash_file returns an error.
    let dst = entry("file.bin", dir.path().join("nonexistent.bin"), 11, old);

    let (progress, mut rx) = new_progress_channel();
    match_trees(
        &[src],
        &[dst],
        Some(progress),
        dirsync::drive::DriveProfile::all_hdd(false),
        &dirsync::sync::CancelToken::default(),
    )
    .unwrap();

    let mut warnings: Vec<String> = vec![];
    while let Ok(ev) = rx.try_recv() {
        if let ProgressEvent::LogEntry(entry) = ev
            && matches!(entry.level, LogLevel::Warning)
        {
            warnings.push(entry.message);
        }
    }

    assert!(
        !warnings.is_empty(),
        "expected a Warning when hash_file fails, got none"
    );
    assert!(
        warnings.iter().any(|w| w.contains("nonexistent.bin")),
        "warning must include the failing path; got: {warnings:?}"
    );
}
