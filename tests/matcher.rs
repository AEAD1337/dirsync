use dirsync::progress::{LogLevel, ProgressEvent, new_progress_channel};
use dirsync::sync::{matcher::MatchResult, matcher::match_trees, walker::FileEntry};
use std::fs;
use std::path::{Path, PathBuf};
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

// --- Helpers for the tests below ---

/// A symlink entry. The walker never resolves a link, so the target is the raw
/// string it read and no file has to exist for it.
fn symlink_entry(rel: &str, target: &str) -> FileEntry {
    FileEntry {
        rel_path: PathBuf::from(rel),
        abs_path: PathBuf::from("/unused"),
        size: 0,
        mtime: SystemTime::UNIX_EPOCH,
        is_dir: false,
        symlink_target: Some(PathBuf::from(target)),
    }
}

fn dir_entry(rel: &str) -> FileEntry {
    FileEntry {
        rel_path: PathBuf::from(rel),
        abs_path: PathBuf::from("/unused"),
        size: 0,
        mtime: SystemTime::UNIX_EPOCH,
        is_dir: true,
        symlink_target: None,
    }
}

fn match_all(src: &[FileEntry], dst: &[FileEntry]) -> dirsync::sync::matcher::MatchOutput {
    match_trees(
        src,
        dst,
        None,
        dirsync::drive::DriveProfile::all_hdd(false),
        &dirsync::sync::CancelToken::default(),
    )
    .unwrap()
}

/// The MatchResult recorded for one relative path.
fn result_for(out: &dirsync::sync::matcher::MatchOutput, rel: &str) -> MatchResult {
    out.matched
        .iter()
        .find(|m| m.src.rel_path == Path::new(rel))
        .unwrap_or_else(|| panic!("{rel} not in the matched set"))
        .result
        .clone()
}

fn orphan_paths(out: &dirsync::sync::matcher::MatchOutput) -> Vec<String> {
    let mut paths: Vec<String> = out
        .orphans
        .iter()
        .map(|o| dirsync::paths::to_slash(&o.dst.rel_path))
        .collect();
    paths.sort();
    paths
}

// --- Symlink matching ---
//
// Symlinks are matched by comparing their targets verbatim: never by hashing,
// and never by following them. These tests build the entries directly, so they
// cover the logic on Windows too, where creating a real symlink needs
// Developer Mode.

#[test]
fn symlink_with_an_unchanged_target_is_identical() {
    let out = match_all(
        &[symlink_entry("link", "target.txt")],
        &[symlink_entry("link", "target.txt")],
    );

    assert!(matches!(result_for(&out, "link"), MatchResult::Identical));
    assert!(out.orphans.is_empty());
}

#[test]
fn symlink_with_a_retargeted_link_is_rewritten() {
    let out = match_all(
        &[symlink_entry("link", "new-target.txt")],
        &[symlink_entry("link", "old-target.txt")],
    );

    // Not a Copy: the planner turns this into a Symlink op that replaces the
    // link in place.
    assert!(matches!(
        result_for(&out, "link"),
        MatchResult::SamePathDifferentContent
    ));
    assert!(out.orphans.is_empty());
}

#[test]
fn symlink_missing_from_dst_is_new() {
    let out = match_all(&[symlink_entry("link", "target.txt")], &[]);

    assert!(matches!(result_for(&out, "link"), MatchResult::NewInSrc));
}

#[test]
fn symlink_landing_on_a_regular_dst_file_replaces_it() {
    let dir = TempDir::new().unwrap();
    let dst_file = dir.path().join("link");
    fs::write(&dst_file, b"a regular file").unwrap();

    let out = match_all(
        &[symlink_entry("link", "target.txt")],
        &[entry("link", dst_file, 14, SystemTime::now())],
    );

    // SamePathDifferentContent, so the planner emits a Symlink op: the
    // executor clears the regular file first. A NewInSrc would emit a Copy
    // and try to write through the path instead.
    assert!(matches!(
        result_for(&out, "link"),
        MatchResult::SamePathDifferentContent
    ));
}

#[test]
fn a_dst_only_symlink_becomes_an_orphan() {
    let out = match_all(&[], &[symlink_entry("stale-link", "somewhere")]);

    assert_eq!(orphan_paths(&out), vec!["stale-link"]);
}

#[test]
fn symlinks_and_regular_files_are_matched_independently() {
    let dir = TempDir::new().unwrap();
    let src_file = dir.path().join("a.txt");
    fs::write(&src_file, b"payload").unwrap();
    let now = SystemTime::now();

    let out = match_all(
        &[
            entry("a.txt", src_file.clone(), 7, now),
            symlink_entry("link", "a.txt"),
        ],
        &[symlink_entry("stale", "gone")],
    );

    assert!(matches!(result_for(&out, "a.txt"), MatchResult::NewInSrc));
    assert!(matches!(result_for(&out, "link"), MatchResult::NewInSrc));
    assert_eq!(orphan_paths(&out), vec!["stale"]);
}

// --- Renamed directories ---

/// A directory rename is detected from the fingerprint of its contents, and
/// the files inside must then match at their post-rename (effective) path
/// instead of being re-detected as individual moves.
#[test]
fn files_nested_deep_inside_a_renamed_directory_match_by_path() {
    let src_dir = TempDir::new().unwrap();
    let dst_dir = TempDir::new().unwrap();
    let now = SystemTime::now();

    let src_file = src_dir.path().join("payload.bin");
    let dst_file = dst_dir.path().join("payload.bin");
    fs::write(&src_file, b"identical content").unwrap();
    fs::write(&dst_file, b"identical content").unwrap();

    let out = match_all(
        &[
            dir_entry("new"),
            dir_entry("new/sub"),
            entry("new/sub/payload.bin", src_file, 17, now),
        ],
        &[
            dir_entry("old"),
            dir_entry("old/sub"),
            entry("old/sub/payload.bin", dst_file, 17, now),
        ],
    );

    assert_eq!(
        out.renamed_dirs.len(),
        1,
        "the directory rename is detected"
    );
    assert_eq!(out.renamed_dirs[0].src_rel, PathBuf::from("new"));
    assert_eq!(out.renamed_dirs[0].dst_rel, PathBuf::from("old"));

    // The file is two levels below the renamed directory: the ancestor walk
    // has to climb past `new/sub` before it finds `new`.
    assert!(matches!(
        result_for(&out, "new/sub/payload.bin"),
        MatchResult::Identical
    ));
    assert!(
        out.orphans.is_empty(),
        "the dir move carries its contents: {:?}",
        orphan_paths(&out)
    );
}

/// A symlink inside a renamed directory travels with the directory move, so it
/// must be matched at its effective path rather than looking new in SRC.
#[test]
fn symlinks_inside_a_renamed_directory_travel_with_it() {
    let src_dir = TempDir::new().unwrap();
    let dst_dir = TempDir::new().unwrap();
    let now = SystemTime::now();

    let src_file = src_dir.path().join("payload.bin");
    let dst_file = dst_dir.path().join("payload.bin");
    fs::write(&src_file, b"identical content").unwrap();
    fs::write(&dst_file, b"identical content").unwrap();

    let out = match_all(
        &[
            dir_entry("new"),
            entry("new/payload.bin", src_file, 17, now),
            symlink_entry("new/link", "payload.bin"),
        ],
        &[
            dir_entry("old"),
            entry("old/payload.bin", dst_file, 17, now),
            symlink_entry("old/link", "payload.bin"),
        ],
    );

    assert_eq!(out.renamed_dirs.len(), 1);
    assert!(matches!(
        result_for(&out, "new/link"),
        MatchResult::Identical
    ));
    assert!(
        out.orphans.is_empty(),
        "the DST symlink is claimed by the rename: {:?}",
        orphan_paths(&out)
    );
}

/// An empty directory has no fingerprint to compare, so it can never be
/// detected as renamed: it is created and removed instead.
#[test]
fn an_empty_directory_is_not_detected_as_a_rename() {
    let out = match_all(&[dir_entry("new")], &[dir_entry("old")]);

    assert!(out.renamed_dirs.is_empty());
}

// --- Move detection edges ---

#[test]
fn empty_files_are_never_matched_as_moves() {
    let dir = TempDir::new().unwrap();
    let src_file = dir.path().join("src-empty");
    let dst_file = dir.path().join("dst-empty");
    fs::write(&src_file, b"").unwrap();
    fs::write(&dst_file, b"").unwrap();

    let out = match_all(
        &[entry("new-name", src_file, 0, SystemTime::now())],
        &[entry("old-name", dst_file, 0, SystemTime::now())],
    );

    // Every empty file hashes the same, so a move match would be arbitrary.
    assert!(matches!(
        result_for(&out, "new-name"),
        MatchResult::NewInSrc
    ));
    assert_eq!(orphan_paths(&out), vec!["old-name"]);
}

#[test]
fn a_relocated_file_is_matched_as_a_move() {
    let dir = TempDir::new().unwrap();
    let src_file = dir.path().join("src.bin");
    let dst_file = dir.path().join("dst.bin");
    fs::write(&src_file, b"the very same bytes").unwrap();
    fs::write(&dst_file, b"the very same bytes").unwrap();
    let now = SystemTime::now();

    let out = match_all(
        &[entry("moved/here.bin", src_file, 19, now)],
        &[entry("was/there.bin", dst_file, 19, now)],
    );

    match result_for(&out, "moved/here.bin") {
        MatchResult::MovedFrom(from) => assert_eq!(from, PathBuf::from("was/there.bin")),
        other => panic!("expected a move, got {other:?}"),
    }
    assert!(out.orphans.is_empty(), "a moved file is not an orphan");
}

#[test]
fn two_src_files_with_one_dst_twin_produce_one_move_and_one_copy() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    let d = dir.path().join("d.bin");
    for p in [&a, &b, &d] {
        fs::write(p, b"duplicate payload").unwrap();
    }
    let now = SystemTime::now();

    let out = match_all(
        &[
            entry("first.bin", a, 17, now),
            entry("second.bin", b, 17, now),
        ],
        &[entry("original.bin", d, 17, now)],
    );

    // The single DST file can only be claimed once; the other SRC file is a
    // plain copy.
    let moves = out
        .matched
        .iter()
        .filter(|m| matches!(m.result, MatchResult::MovedFrom(_)))
        .count();
    let news = out
        .matched
        .iter()
        .filter(|m| matches!(m.result, MatchResult::NewInSrc))
        .count();
    assert_eq!((moves, news), (1, 1));
    assert!(out.orphans.is_empty());
}
