use dirsync::progress::{ProgressEvent, ScanPhase, new_progress_channel};
use dirsync::sync::CancelToken;
use dirsync::sync::walker::{build_excludes, walk};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::sync::watch;

fn write_file(dir: &Path, rel: &str, content: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn no_excludes() -> dirsync::sync::walker::ExcludeSet {
    build_excludes(&[]).unwrap()
}

fn not_cancelled() -> CancelToken {
    CancelToken::new(None)
}

/// Relative paths of a walk result, slash-normalized and sorted so the
/// assertions read the same on Windows and Unix.
fn rel_paths(entries: &[dirsync::sync::walker::FileEntry]) -> Vec<String> {
    let mut paths: Vec<String> = entries
        .iter()
        .map(|e| dirsync::paths::to_slash(&e.rel_path))
        .collect();
    paths.sort();
    paths
}

fn drain(rx: &mut broadcast::Receiver<ProgressEvent>) -> Vec<ProgressEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[test]
fn walk_collects_files_and_directories() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "a.txt", b"hello");
    write_file(root.path(), "sub/b.txt", b"world!");

    let entries = walk(root.path(), &no_excludes(), "src", None, &not_cancelled()).unwrap();

    assert_eq!(rel_paths(&entries), vec!["a.txt", "sub", "sub/b.txt"]);
}

#[test]
fn walk_records_size_and_dir_flag() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "a.txt", b"hello");
    fs::create_dir(root.path().join("empty")).unwrap();

    let entries = walk(root.path(), &no_excludes(), "src", None, &not_cancelled()).unwrap();

    let file = entries.iter().find(|e| !e.is_dir).unwrap();
    assert_eq!(file.size, 5);
    assert!(file.symlink_target.is_none());
    assert!(file.abs_path.ends_with("a.txt"));

    let dir = entries.iter().find(|e| e.is_dir).unwrap();
    // Directories are reported with size 0 whatever the filesystem claims.
    assert_eq!(dir.size, 0);
}

#[test]
fn walk_skips_the_root_itself() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "a.txt", b"x");

    let entries = walk(root.path(), &no_excludes(), "src", None, &not_cancelled()).unwrap();

    assert!(entries.iter().all(|e| !e.rel_path.as_os_str().is_empty()));
}

#[test]
fn walk_prunes_excluded_directories() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "keep.txt", b"x");
    write_file(root.path(), "node_modules/dep/index.js", b"x");

    let excludes = build_excludes(&["node_modules".to_string()]).unwrap();
    let entries = walk(root.path(), &excludes, "src", None, &not_cancelled()).unwrap();

    assert_eq!(rel_paths(&entries), vec!["keep.txt"]);
}

#[test]
fn walk_applies_glob_excludes_per_component() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "keep.txt", b"x");
    write_file(root.path(), "scratch.tmp", b"x");

    let excludes = build_excludes(&["*.tmp".to_string()]).unwrap();
    let entries = walk(root.path(), &excludes, "src", None, &not_cancelled()).unwrap();

    assert_eq!(rel_paths(&entries), vec!["keep.txt"]);
}

#[test]
fn walk_excludes_dirsync_staging_files_without_configuration() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "keep.txt", b"x");
    write_file(root.path(), "leftover.__dirsync_tmp__", b"x");
    write_file(root.path(), "case.__dirsync_case__", b"x");

    let entries = walk(root.path(), &no_excludes(), "src", None, &not_cancelled()).unwrap();

    assert_eq!(rel_paths(&entries), vec!["keep.txt"]);
}

#[test]
fn exclude_set_matches_builtin_and_user_patterns() {
    let excludes = build_excludes(&["*.bak".to_string()]).unwrap();

    assert!(excludes.is_match(std::ffi::OsStr::new("System Volume Information")));
    assert!(excludes.is_match(std::ffi::OsStr::new("$Recycle.Bin")));
    assert!(excludes.is_match(std::ffi::OsStr::new("notes.bak")));
    assert!(!excludes.is_match(std::ffi::OsStr::new("notes.txt")));
}

#[tokio::test]
async fn walk_emits_scan_progress_and_a_final_done() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "a.txt", b"x");

    let (progress, mut rx) = new_progress_channel();
    walk(
        root.path(),
        &no_excludes(),
        "dst",
        Some(Arc::clone(&progress)),
        &not_cancelled(),
    )
    .unwrap();

    let events = drain(&mut rx);
    let walking: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ProgressEvent::ScanProgress {
                phase: ScanPhase::Walking { side },
                path,
            } => Some((side.clone(), path.clone())),
            _ => None,
        })
        .collect();

    assert!(!walking.is_empty(), "expected at least the final event");
    assert!(walking.iter().all(|(side, _)| side == "dst"));
    assert_eq!(
        walking.last().unwrap().1.as_deref(),
        Some("Done."),
        "the walk always closes with a Done. event"
    );
}

#[tokio::test]
async fn walk_returns_the_cancel_sentinel_when_cancelled() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "a.txt", b"x");

    let (_tx, rx) = watch::channel(true);
    let cancel = CancelToken::new(Some(rx));
    assert!(cancel.is_cancelled());

    let err = walk(root.path(), &no_excludes(), "src", None, &cancel).unwrap_err();

    // The engine and the GUI both match on this exact string to tell a cancel
    // apart from a real failure.
    assert_eq!(err.to_string(), "cancelled");
}

#[tokio::test]
async fn walk_logs_a_warning_instead_of_failing_on_an_unreadable_root() {
    let root = TempDir::new().unwrap();
    let missing = root.path().join("does-not-exist");

    let (progress, mut rx) = new_progress_channel();
    let entries = walk(
        &missing,
        &no_excludes(),
        "src",
        Some(Arc::clone(&progress)),
        &not_cancelled(),
    )
    .unwrap();

    assert!(entries.is_empty());
    let logged = drain(&mut rx)
        .iter()
        .any(|e| matches!(e, ProgressEvent::LogEntry(l) if l.message.contains("Walk error")));
    assert!(logged, "the walk error should reach the log panel");
}

#[test]
fn walk_without_progress_still_survives_an_unreadable_root() {
    let root = TempDir::new().unwrap();
    let missing = root.path().join("does-not-exist");

    // The no-progress branch prints to stderr instead of emitting a log event.
    let entries = walk(&missing, &no_excludes(), "src", None, &not_cancelled()).unwrap();

    assert!(entries.is_empty());
}

#[test]
fn walk_records_symlinks_without_following_them() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "target.txt", b"payload");

    let link = root.path().join("link.txt");
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink("target.txt", &link).is_ok();
    // Windows only allows this with Developer Mode or elevation: when the
    // symlink cannot be created there is nothing to assert, so skip.
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file("target.txt", &link).is_ok();
    if !created {
        return;
    }

    let entries = walk(root.path(), &no_excludes(), "src", None, &not_cancelled()).unwrap();

    let link_entry = entries
        .iter()
        .find(|e| e.rel_path == Path::new("link.txt"))
        .expect("symlink should be walked");
    assert_eq!(
        link_entry.symlink_target.as_deref(),
        Some(Path::new("target.txt")),
        "the raw link target is kept, not the resolved file"
    );
    // A symlink is never reported as a directory, and never carries the
    // target's size: following it is exactly what the walker must not do.
    assert!(!link_entry.is_dir);
    assert_eq!(link_entry.size, 0);
}

/// Windows refuses symlink creation without Developer Mode, but a directory
/// junction is a reparse point any user can create, and the walker treats it
/// as a symlink. This keeps the symlink branch covered on Windows too.
#[test]
#[cfg(windows)]
fn walk_records_a_directory_junction_without_descending_into_it() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join("target")).unwrap();
    write_file(root.path(), "target/inside.txt", b"payload");

    let link = root.path().join("junction");
    let created = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            link.to_str().unwrap(),
            root.path().join("target").to_str().unwrap(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !created {
        return;
    }

    let entries = walk(root.path(), &no_excludes(), "src", None, &not_cancelled()).unwrap();
    let paths = rel_paths(&entries);

    let junction = entries
        .iter()
        .find(|e| e.rel_path == Path::new("junction"))
        .expect("the junction should be walked");
    assert!(junction.symlink_target.is_some());
    assert!(!junction.is_dir);
    assert_eq!(junction.size, 0);
    // Following it would enumerate the target's contents a second time.
    assert!(
        !paths.iter().any(|p| p == "junction/inside.txt"),
        "the walker must not descend into a reparse point: {paths:?}"
    );
}

/// A symlink pointing nowhere still has to be recorded: the target is copied
/// verbatim, and resolving it is exactly what the walker must not do.
#[test]
fn walk_records_a_dangling_symlink() {
    let root = TempDir::new().unwrap();

    let link = root.path().join("dangling");
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink("no-such-file", &link).is_ok();
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file("no-such-file", &link).is_ok();
    if !created {
        return;
    }

    let entries = walk(root.path(), &no_excludes(), "src", None, &not_cancelled()).unwrap();

    let entry = entries
        .iter()
        .find(|e| e.rel_path == Path::new("dangling"))
        .expect("a dangling symlink is still an entry");
    assert_eq!(
        entry.symlink_target.as_deref(),
        Some(Path::new("no-such-file"))
    );
    assert_eq!(entry.mtime, std::time::SystemTime::UNIX_EPOCH);
}

/// Excludes are matched per component, so a pattern containing a separator can
/// never match and the subtree stays in the walk.
#[test]
fn walk_keeps_paths_a_separator_pattern_cannot_match() {
    let root = TempDir::new().unwrap();
    write_file(root.path(), "build/temp/file.txt", b"x");

    let excludes = build_excludes(&["build/temp".to_string()]).unwrap();
    let entries = walk(root.path(), &excludes, "src", None, &not_cancelled()).unwrap();

    assert_eq!(
        rel_paths(&entries),
        vec!["build", "build/temp", "build/temp/file.txt"]
    );
}

/// The walk is cancellable between entries, not just at phase boundaries: a
/// token that flips after the walk starts still aborts it, and it aborts with
/// an error rather than the entries collected so far. A truncated DST walk
/// would make real files look like orphans, and orphans get deleted.
#[tokio::test]
async fn walk_aborts_rather_than_returning_a_partial_tree() {
    let root = TempDir::new().unwrap();
    for i in 0..50 {
        write_file(root.path(), &format!("f{i}.txt"), b"x");
    }

    let (tx, rx) = watch::channel(false);
    let cancel = CancelToken::new(Some(rx));
    tx.send(true).unwrap();

    assert!(walk(root.path(), &no_excludes(), "src", None, &cancel).is_err());
}
