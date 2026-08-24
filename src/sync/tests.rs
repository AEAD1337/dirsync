use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use filetime::{FileTime, set_file_mtime};
use tempfile::TempDir;

use crate::config::AppConfig;
use crate::progress::{ProgressEvent, ProgressState};
use crate::sync::SyncEngine;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_config() -> Arc<AppConfig> {
    Arc::new(AppConfig::default())
}

/// Write a file with deterministic content based on `tag` and a given size.
fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// Bump the mtime of `path` by +4 s so the fast-path (mtime equality) detects a change.
fn touch_newer(path: &Path) {
    let meta = fs::metadata(path).unwrap();
    let mtime = meta.modified().unwrap();
    let newer = mtime + Duration::from_secs(4);
    set_file_mtime(path, FileTime::from_system_time(newer)).unwrap();
}

/// Set the mtime of `path` to 10 seconds in the past so the matcher is forced
/// to hash the file rather than declaring it Identical via mtime alone.
fn set_mtime_to_past(path: &Path) {
    let past = SystemTime::now() - Duration::from_secs(10);
    set_file_mtime(path, FileTime::from_system_time(past)).unwrap();
}

async fn run_sync(src: &Path, dst: &Path) {
    let cfg = default_config();
    let engine = SyncEngine::new(src.to_path_buf(), dst.to_path_buf(), cfg);
    let plan = engine.preview(None, None).await.unwrap();

    let (tx, _rx) = tokio::sync::broadcast::channel::<ProgressEvent>(64);
    let progress = Arc::new(ProgressState::new(tx));
    let (_pause_tx, pause_rx) = tokio::sync::watch::channel(false);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    engine.run(plan, progress, false, pause_rx, cancel_rx).await;
}

/// Run a sync with an explicit drive profile: exercises the serial hashing
/// and serial copy paths that the default (SSD) profile never reaches.
async fn run_sync_with_drives(src: &Path, dst: &Path, drives: crate::drive::DriveProfile) {
    let engine =
        SyncEngine::new(src.to_path_buf(), dst.to_path_buf(), default_config()).with_drives(drives);
    let plan = engine.preview(None, None).await.unwrap();

    let (tx, _rx) = tokio::sync::broadcast::channel::<ProgressEvent>(64);
    let progress = Arc::new(ProgressState::new(tx));
    let (_pause_tx, pause_rx) = tokio::sync::watch::channel(false);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    engine.run(plan, progress, false, pause_rx, cancel_rx).await;
}

/// Assert that DST is an exact mirror of SRC: same relative files, same content.
fn assert_mirror(src: &Path, dst: &Path) {
    let src_files = collect_files(src);
    let dst_files = collect_files(dst);

    let mut src_rel: Vec<_> = src_files.keys().cloned().collect();
    let mut dst_rel: Vec<_> = dst_files.keys().cloned().collect();
    src_rel.sort();
    dst_rel.sort();

    assert_eq!(
        src_rel, dst_rel,
        "file set mismatch\n  src: {src_rel:?}\n  dst: {dst_rel:?}"
    );

    for rel in &src_rel {
        let s = &src_files[rel];
        let d = &dst_files[rel];
        assert_eq!(s, d, "content mismatch for {rel}");
    }
}

/// Recursively collect all non-dir files under `root` as relative-path → content.
fn collect_files(root: &Path) -> HashMap<String, Vec<u8>> {
    let mut map = HashMap::new();
    collect_recursive(root, root, &mut map);
    map
}

fn collect_recursive(root: &Path, dir: &Path, map: &mut HashMap<String, Vec<u8>>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_recursive(root, &path, map);
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let content = fs::read(&path).unwrap();
            map.insert(rel, content);
        }
    }
}

// ---------------------------------------------------------------------------
// Basic sanity: fresh sync
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_basic_copy() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("a.txt"), b"hello");
    write_file(&src.join("sub/b.txt"), b"world");

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Bug 1: circular swap (different sizes - old code would clobber content)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_circular_swap_different_sizes() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let a_content = b"AAAAAAAAAA".as_ref(); // 10 bytes
    let b_content = b"BBBBBBBBBBBBBBBBBBBB".as_ref(); // 20 bytes

    // SRC: a.txt=A, b.txt=B
    write_file(&src.join("a.txt"), a_content);
    write_file(&src.join("b.txt"), b_content);

    // DST: a.txt=B (20 bytes), b.txt=A (10 bytes): they have been swapped.
    // The matcher will detect: a.txt in SRC matches b.txt in DST (same hash),
    // and b.txt in SRC matches a.txt in DST.
    write_file(&dst.join("a.txt"), b_content);
    write_file(&dst.join("b.txt"), a_content);
    // Different sizes: move detection triggers via hash; no mtime adjustment needed.

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

#[tokio::test]
async fn test_circular_swap_same_size() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    // Same size but different content: forces a hash-based comparison.
    let a_content = b"AAAAAAAAAA".as_ref();
    let b_content = b"BBBBBBBBBB".as_ref();

    write_file(&src.join("a.txt"), a_content);
    write_file(&src.join("b.txt"), b_content);

    // Same size: force hash comparison by making DST mtimes old.
    write_file(&dst.join("a.txt"), b_content);
    set_mtime_to_past(&dst.join("a.txt"));
    write_file(&dst.join("b.txt"), a_content);
    set_mtime_to_past(&dst.join("b.txt"));

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Bug 1b: 3-way cycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_three_way_cycle() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let a = b"CONTENT_A_______".as_ref(); // 16 bytes
    let b = b"CONTENT_B_______".as_ref();
    let c = b"CONTENT_C_______".as_ref();

    // SRC: a=A, b=B, c=C
    write_file(&src.join("a.txt"), a);
    write_file(&src.join("b.txt"), b);
    write_file(&src.join("c.txt"), c);

    // DST: a=C, b=A, c=B  (rotated: needs a→b, b→c, c→a)
    // Same size: force hash comparison by making DST mtimes old.
    write_file(&dst.join("a.txt"), c);
    set_mtime_to_past(&dst.join("a.txt"));
    write_file(&dst.join("b.txt"), a);
    set_mtime_to_past(&dst.join("b.txt"));
    write_file(&dst.join("c.txt"), b);
    set_mtime_to_past(&dst.join("c.txt"));

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Bug 2: move chain + orphan delete conflict
// ---------------------------------------------------------------------------

/// SRC: a.txt (X bytes), b.txt (Y bytes)
/// DST: a.txt (Y bytes = SRC b), b.txt (Z unique bytes), c.txt (X bytes = SRC a)
///
/// Matcher produces:
///   SRC a → move from DST c.txt   (X content)
///   SRC b → move from DST a.txt   (Y content)
///   DST b.txt is an orphan        → Delete b.txt
///
/// The old bug: moves execute a→c→?, then Delete b.txt removes whatever ended
/// up there. With the fix, the Delete should not clobber a correctly-moved file.
#[tokio::test]
async fn test_move_chain_orphan_delete_no_conflict() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let x = b"CONTENT_X_____".as_ref(); // 14 bytes
    let y = b"CONTENT_Y_________".as_ref(); // 18 bytes
    let z = b"UNIQUE_ORPHAN_ZZZ".as_ref(); // 17 bytes: unique, no match in SRC

    // SRC: a.txt=X, b.txt=Y
    write_file(&src.join("a.txt"), x);
    write_file(&src.join("b.txt"), y);

    // DST: a.txt=Y, b.txt=Z (orphan), c.txt=X
    write_file(&dst.join("a.txt"), y);
    write_file(&dst.join("b.txt"), z);
    write_file(&dst.join("c.txt"), x);

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Bug 3: move chain ordering (no cycle, but must run in dependency order)
// ---------------------------------------------------------------------------

/// DST has a chain: file1 → needs to move to where file2 currently is,
/// and file2 needs to move elsewhere.  Alphabetical order would execute
/// file1's move first, clobbering file2's source.
#[tokio::test]
async fn test_move_chain_ordering() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let content_a = b"FILE_A_CONTENT__".as_ref(); // 16 bytes
    let content_b = b"FILE_B_CONTENT__".as_ref(); // 16 bytes (same size, diff content)

    // SRC: a.txt=A, b.txt=B
    write_file(&src.join("a.txt"), content_a);
    write_file(&src.join("b.txt"), content_b);

    // DST: a.txt=B (SRC's b), b.txt=A (SRC's a).
    // Same size: force hash comparison by making DST mtimes old.
    write_file(&dst.join("a.txt"), content_b);
    set_mtime_to_past(&dst.join("a.txt"));
    write_file(&dst.join("b.txt"), content_a);
    set_mtime_to_past(&dst.join("b.txt"));

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Existing file changed in-place (overwrite, not move)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_overwrite_changed_file() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("file.txt"), b"version 1");
    run_sync(&src, &dst).await;

    // Modify SRC and bump mtime so the fast path detects it.
    write_file(&src.join("file.txt"), b"version 2 longer!!!");
    touch_newer(&src.join("file.txt"));

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Delete orphan that has no move conflict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_true_orphan() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("keep.txt"), b"kept");
    write_file(&dst.join("keep.txt"), b"kept");
    write_file(&dst.join("orphan.txt"), b"should be deleted");

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
    assert!(
        !dst.join("orphan.txt").exists(),
        "orphan should have been deleted"
    );
}

// ---------------------------------------------------------------------------
// Dir rename with new file added inside
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dir_rename_with_new_file() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    // SRC: renamed_dir/old.txt + renamed_dir/new.txt (new file)
    write_file(&src.join("renamed_dir/old.txt"), b"old content");
    write_file(&src.join("renamed_dir/new.txt"), b"brand new");

    // DST: original_dir/old.txt (same content as SRC renamed_dir/old.txt)
    write_file(&dst.join("original_dir/old.txt"), b"old content");

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Same-path file must never be matched as a move from a different path.
// Regression: when SRC P and DST P both exist and appear identical, but DST
// also contains another file Q with the same hash, the matcher must not
// generate Move(Q→P) for P: that would corrupt or spuriously re-trigger ops.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_same_path_not_matched_as_move() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let content = b"SHARED_CONTENT__"; // 16 bytes

    // SRC: only a.txt with this content.
    write_file(&src.join("a.txt"), content);

    // DST: a.txt AND b.txt both have the same content. b.txt is an orphan.
    // Without the fix, the matcher could match SRC a.txt against DST b.txt
    // (same size, same hash) and generate Move(b.txt → a.txt): which makes
    // DST a.txt an orphan and creates spurious ops on every subsequent preview.
    write_file(&dst.join("a.txt"), content);
    set_mtime_to_past(&dst.join("a.txt")); // force hash comparison
    write_file(&dst.join("b.txt"), content);

    run_sync(&src, &dst).await;
    // DST must mirror SRC: only a.txt, b.txt deleted as orphan.
    assert_mirror(&src, &dst);

    // Second preview must show zero ops (idempotent).
    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();
    assert_eq!(plan.copy_count, 0, "no copies on re-preview");
    assert_eq!(plan.move_count, 0, "no moves on re-preview");
    assert_eq!(plan.delete_count, 0, "no deletes on re-preview");
    assert_eq!(plan.overwrite_count, 0, "no overwrites on re-preview");
}

// ---------------------------------------------------------------------------
// Zero-byte files must never be matched as moves of each other
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_zero_byte_files_not_moved() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    // SRC: two empty files at different paths.
    write_file(&src.join("a.txt"), b"");
    write_file(&src.join("sub/b.txt"), b"");

    // DST: only one of them exists (sub/b.txt is new in SRC).
    write_file(&dst.join("a.txt"), b"");

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);

    // Second preview must show zero ops.
    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();
    assert_eq!(
        plan.move_count, 0,
        "zero-byte files must not generate move ops"
    );
    assert_eq!(
        plan.copy_count + plan.overwrite_count + plan.delete_count,
        0,
        "no other ops on re-preview"
    );
}

// ---------------------------------------------------------------------------
// Many same-hash files: new SRC file must not steal a DST file that already
// has a same-path SRC counterpart.
//
// Real scenario: dozens of files in SRC/DST share the same hash (e.g. default
// game-stat blobs). SRC gains new files with the same hash. The matcher must
// not match a new SRC file to a DST file that already has its own SRC
// counterpart at the same path, even when the new file is processed first.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_same_hash_many_files_new_added() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    // All files share identical content (same hash). Exact size doesn't matter.
    let content = b"SAME_CONTENT_BLOB_XXXXXXXXXXXXXX".as_ref();
    assert_eq!(content.len(), 32);

    // SRC has files a00..a19 (alphabetically: a00 < a05 < a10 < a19 etc.)
    // plus two new files: b_new1 and b_new2 (alphabetically these come after a*).
    // DST has a00..a19 only.
    //
    // Key: "a05" is alphabetically before "b_new", so when the matcher processes
    // b_new1 and b_new2 (no same-path DST), it must not steal a05 etc. as move
    // sources: those are reserved for their own same-path SRC matches.
    for i in 0..20usize {
        let name = format!("a{:02}.bin", i);
        write_file(&src.join(&name), content);
        write_file(&dst.join(&name), content);
        set_mtime_to_past(&dst.join(&name));
    }
    write_file(&src.join("b_new1.bin"), content);
    write_file(&src.join("b_new2.bin"), content);

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);

    // Second preview must show zero ops.
    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();
    assert_eq!(plan.move_count, 0, "no moves on re-preview");
    assert_eq!(
        plan.copy_count + plan.overwrite_count + plan.delete_count,
        0,
        "no other ops on re-preview"
    );
}

async fn run_sync_with_config(src: &Path, dst: &Path, config: Arc<AppConfig>) {
    let engine = SyncEngine::new(src.to_path_buf(), dst.to_path_buf(), config);
    let plan = engine.preview(None, None).await.unwrap();
    let (tx, _rx) = tokio::sync::broadcast::channel::<ProgressEvent>(64);
    let progress = Arc::new(ProgressState::new(tx));
    let (_pause_tx, pause_rx) = tokio::sync::watch::channel(false);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    engine.run(plan, progress, false, pause_rx, cancel_rx).await;
}

// ---------------------------------------------------------------------------
// Idempotent: running sync twice yields same result
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_idempotent() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("a/b/c.txt"), b"deep file");
    write_file(&src.join("x.txt"), b"root file");

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);

    // Second run: nothing should change, DST should still mirror SRC.
    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Type conflict: SRC has a directory where DST has a plain file of the same name.
// The executor must remove the file before creating the directory.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_type_conflict_dir_over_file() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    // SRC: foo is a directory containing bar.txt
    write_file(&src.join("foo/bar.txt"), b"bar content");
    // DST: foo is a plain file, not a directory
    write_file(&dst.join("foo"), b"wrong type");

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);
}

// ---------------------------------------------------------------------------
// Exclude patterns
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_exclude_file_not_copied() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("keep.txt"), b"keep");
    write_file(&src.join("skip.tmp"), b"temporary");

    let cfg = Arc::new(AppConfig {
        exclude_patterns: vec!["*.tmp".into()],
        ..AppConfig::default()
    });
    run_sync_with_config(&src, &dst, cfg).await;

    assert!(dst.join("keep.txt").exists());
    assert!(
        !dst.join("skip.tmp").exists(),
        "excluded file must not be copied to DST"
    );
}

#[tokio::test]
async fn test_exclude_dir_not_copied() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("keep.txt"), b"keep");
    write_file(&src.join(".git/HEAD"), b"ref: refs/heads/main");
    write_file(&src.join(".git/config"), b"[core]");

    let cfg = Arc::new(AppConfig {
        exclude_patterns: vec![".git".into()],
        ..AppConfig::default()
    });
    run_sync_with_config(&src, &dst, cfg).await;

    assert!(dst.join("keep.txt").exists());
    assert!(
        !dst.join(".git").exists(),
        "excluded directory must not be copied to DST"
    );
}

/// Excluded patterns suppress both the SRC and DST walks, so a DST file
/// matching the pattern is invisible to the orphan detector and is left in place.
#[tokio::test]
async fn test_exclude_preserves_existing_dst_file() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("keep.txt"), b"keep");
    write_file(&dst.join("keep.txt"), b"keep");
    // This file has no SRC counterpart: normally it would be deleted as an orphan.
    // The exclude pattern makes the DST walker blind to it, so it survives.
    write_file(&dst.join("local.tmp"), b"preserved by exclude");

    let cfg = Arc::new(AppConfig {
        exclude_patterns: vec!["*.tmp".into()],
        ..AppConfig::default()
    });
    run_sync_with_config(&src, &dst, cfg).await;

    assert!(dst.join("keep.txt").exists());
    assert!(
        dst.join("local.tmp").exists(),
        "excluded DST file must not be deleted"
    );
}

// ---------------------------------------------------------------------------
// mtime tolerance: files with mtime within ±3 s are Identical without hashing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mtime_within_tolerance_is_identical() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let content = b"same content in both";
    write_file(&src.join("file.txt"), content);
    write_file(&dst.join("file.txt"), content);

    // 1 s difference: inside the 3 s tolerance window → Identical without hashing.
    let past = SystemTime::now() - Duration::from_secs(1);
    set_file_mtime(dst.join("file.txt"), FileTime::from_system_time(past)).unwrap();

    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(
        plan.identical_count, 1,
        "file within mtime tolerance must be Identical"
    );
    assert_eq!(
        plan.overwrite_count, 0,
        "must not overwrite a file that is within mtime tolerance"
    );
    assert_eq!(plan.copy_count, 0);
}

#[tokio::test]
async fn test_mtime_outside_tolerance_same_content_emits_touch() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let content = b"same content in both";
    write_file(&src.join("file.txt"), content);
    write_file(&dst.join("file.txt"), content);

    // 5 s difference: outside the 3 s tolerance → must hash both, confirm
    // content is identical, and emit a TouchMtime op (not an overwrite).
    let past = SystemTime::now() - Duration::from_secs(5);
    set_file_mtime(dst.join("file.txt"), FileTime::from_system_time(past)).unwrap();

    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();

    assert_eq!(
        plan.touch_count, 1,
        "same content beyond mtime tolerance must produce a TouchMtime op"
    );
    assert_eq!(plan.identical_count, 0);
    assert_eq!(plan.overwrite_count, 0);
    assert_eq!(plan.copy_count, 0);
}

#[tokio::test]
async fn test_mtime_diverged_touch_corrects_dst_mtime() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let content = b"same content in both";
    write_file(&src.join("file.txt"), content);
    write_file(&dst.join("file.txt"), content);

    // Push DST mtime 10 s into the past: well outside tolerance.
    let past = SystemTime::now() - Duration::from_secs(10);
    set_file_mtime(dst.join("file.txt"), FileTime::from_system_time(past)).unwrap();

    run_sync(&src, &dst).await;

    // After sync the DST mtime must match SRC (within 1 s of filesystem precision).
    let src_mtime = fs::metadata(src.join("file.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let dst_mtime = fs::metadata(dst.join("file.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let diff = if src_mtime >= dst_mtime {
        src_mtime.duration_since(dst_mtime).unwrap()
    } else {
        dst_mtime.duration_since(src_mtime).unwrap()
    };
    assert!(
        diff <= Duration::from_secs(1),
        "DST mtime must be corrected to match SRC after TouchMtime; diff={diff:?}"
    );

    // A second preview must produce zero ops (idempotent).
    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();
    assert_eq!(
        plan.touch_count + plan.copy_count + plan.overwrite_count + plan.delete_count,
        0,
        "no ops on re-preview after mtime correction"
    );
}

// ---------------------------------------------------------------------------
// Dir rename idempotency: second preview after a rename must show zero ops.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dir_rename_idempotent() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("new_name/file.txt"), b"content");
    write_file(&dst.join("old_name/file.txt"), b"content");

    // First sync: rename old_name → new_name.
    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);

    // Second preview: DST already matches SRC: must produce zero ops.
    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();
    assert_eq!(
        plan.move_count, 0,
        "no moves on re-preview after dir rename"
    );
    assert_eq!(
        plan.copy_count + plan.overwrite_count + plan.delete_count,
        0,
        "no other ops on re-preview after dir rename"
    );
}

// ---------------------------------------------------------------------------
// Empty SRC: every DST file/dir becomes an orphan and must be removed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_empty_src_deletes_all_dst() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    fs::create_dir_all(&src).unwrap();
    write_file(&dst.join("orphan1.txt"), b"bye");
    write_file(&dst.join("sub/orphan2.txt"), b"bye too");

    run_sync(&src, &dst).await;

    assert!(
        collect_files(&dst).is_empty(),
        "DST must be empty after syncing from an empty SRC"
    );
}

// ---------------------------------------------------------------------------
// Deep orphan directory cleanup: RmDir must handle multi-level trees correctly.
// The planner sorts RmDir ops deepest-first so each remove_dir call finds an
// empty directory.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rmdir_deep_orphan_dirs() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&src.join("keep.txt"), b"keep");
    write_file(&dst.join("keep.txt"), b"keep");
    // Three-level deep orphan subtree that SRC has no equivalent for.
    write_file(&dst.join("a/b/c/deep.txt"), b"orphan deep file");

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);

    assert!(
        !dst.join("a").exists(),
        "orphan directory tree must be fully removed"
    );
}

// ---------------------------------------------------------------------------
// File moved across directories (not a dir rename): individual file relocation
// must be detected by the file-level move detector.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_file_moved_across_dirs() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    // SRC: file has moved from sub_a/ to sub_b/.
    write_file(&src.join("sub_a/unchanged.txt"), b"unchanged");
    write_file(
        &src.join("sub_b/relocated.txt"),
        b"UNIQUE_RELOCATED_CONTENT",
    );

    // DST: file still at its old location inside sub_a/.
    write_file(&dst.join("sub_a/unchanged.txt"), b"unchanged");
    write_file(
        &dst.join("sub_a/relocated.txt"),
        b"UNIQUE_RELOCATED_CONTENT",
    );

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);

    // Second preview: zero ops.
    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();
    assert_eq!(
        plan.move_count + plan.copy_count + plan.delete_count + plan.overwrite_count,
        0,
        "no ops on re-preview after cross-dir file move"
    );
}

// ---------------------------------------------------------------------------
// Duplicate hash in SRC: two SRC files with the same content where one already
// exists in DST. The DST copy must not be claimed as a move source for the new
// SRC file: it must be reserved for its own same-path match.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_duplicate_hash_in_src_copies_not_moves() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    let content = b"IDENTICAL_CONTENT_IN_BOTH_FILES";

    // SRC: two files with identical content.
    write_file(&src.join("a.txt"), content);
    write_file(&src.join("b.txt"), content);

    // DST: only a.txt (same content, old mtime to force hash comparison).
    write_file(&dst.join("a.txt"), content);
    set_mtime_to_past(&dst.join("a.txt"));

    run_sync(&src, &dst).await;
    assert_mirror(&src, &dst);

    // a.txt must still have its original content (not moved away as b.txt's source).
    let a_content = fs::read(dst.join("a.txt")).unwrap();
    assert_eq!(
        a_content, content,
        "a.txt must not have been used as a move source"
    );

    // Second preview: zero ops.
    let cfg = default_config();
    let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
    let plan = engine.preview(None, None).await.unwrap();
    assert_eq!(
        plan.copy_count + plan.move_count + plan.overwrite_count + plan.delete_count,
        0,
        "no ops on re-preview after duplicate-hash sync"
    );
}

// ---------------------------------------------------------------------------
// Symlink scenarios (Unix only: creating symlinks on Windows requires
// Developer Mode or elevated privileges, making portable tests impractical).
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod symlink_tests {
    use super::*;
    use std::os::unix::fs as unix_fs;

    fn assert_is_symlink(path: &Path) {
        let meta = fs::symlink_metadata(path).expect("path should exist");
        assert!(
            meta.file_type().is_symlink(),
            "{path:?} should be a symlink"
        );
    }

    fn read_link_target(path: &Path) -> std::path::PathBuf {
        fs::read_link(path).expect("should be a symlink")
    }

    // New symlink in SRC is created at the same path in DST.
    #[tokio::test]
    async fn test_symlink_copied_to_dst() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        unix_fs::symlink("../target.txt", src.join("link.txt")).unwrap();

        run_sync(&src, &dst).await;

        assert_is_symlink(&dst.join("link.txt"));
        assert_eq!(
            read_link_target(&dst.join("link.txt")),
            std::path::Path::new("../target.txt")
        );
    }

    // Symlink target changes in SRC: DST must be updated to the new target.
    #[tokio::test]
    async fn test_symlink_target_updated() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        unix_fs::symlink("old_target.txt", src.join("link.txt")).unwrap();
        unix_fs::symlink("old_target.txt", dst.join("link.txt")).unwrap();

        // Update SRC symlink to a new target.
        fs::remove_file(src.join("link.txt")).unwrap();
        unix_fs::symlink("new_target.txt", src.join("link.txt")).unwrap();

        run_sync(&src, &dst).await;

        assert_is_symlink(&dst.join("link.txt"));
        assert_eq!(
            read_link_target(&dst.join("link.txt")),
            std::path::Path::new("new_target.txt"),
            "DST symlink must point to the updated target"
        );
    }

    // Symlink removed from SRC: DST copy must be deleted as an orphan.
    #[tokio::test]
    async fn test_symlink_orphan_deleted() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        unix_fs::symlink("target.txt", dst.join("link.txt")).unwrap();

        run_sync(&src, &dst).await;

        assert!(
            !dst.join("link.txt").exists() && fs::symlink_metadata(dst.join("link.txt")).is_err(),
            "orphan symlink must be deleted from DST"
        );
    }

    // SRC has a symlink where DST has a regular file: symlink must replace the file.
    #[tokio::test]
    async fn test_symlink_replaces_regular_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        unix_fs::symlink("target.txt", src.join("item")).unwrap();
        write_file(&dst.join("item"), b"i am a regular file");

        run_sync(&src, &dst).await;

        assert_is_symlink(&dst.join("item"));
        assert_eq!(
            read_link_target(&dst.join("item")),
            std::path::Path::new("target.txt")
        );
    }

    // SRC has a regular file where DST has a symlink: file must replace the symlink.
    #[tokio::test]
    async fn test_regular_file_replaces_symlink() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        write_file(&src.join("item"), b"i am a regular file");
        unix_fs::symlink("target.txt", dst.join("item")).unwrap();

        run_sync(&src, &dst).await;

        let meta = fs::symlink_metadata(dst.join("item")).unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "symlink must be replaced by the regular file"
        );
        assert_eq!(fs::read(dst.join("item")).unwrap(), b"i am a regular file");
    }

    // Identical symlink in both SRC and DST: second sync must produce zero ops.
    #[tokio::test]
    async fn test_symlink_second_sync_is_noop() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        unix_fs::symlink("target.txt", src.join("link.txt")).unwrap();

        run_sync(&src, &dst).await;
        assert_is_symlink(&dst.join("link.txt"));

        let cfg = default_config();
        let engine = SyncEngine::new(src.clone(), dst.clone(), cfg);
        let plan = engine.preview(None, None).await.unwrap();
        assert_eq!(
            plan.symlink_count + plan.copy_count + plan.overwrite_count + plan.delete_count,
            0,
            "identical symlink must produce zero ops on re-preview"
        );
        assert_eq!(plan.identical_count, 1);
    }

    // SRC has a real directory where DST has a symlink *to a directory*
    // outside the mirror. The MkDir must replace the link: create_dir_all
    // would silently follow it, and the copies below would then land outside
    // dst_root while the run reports success.
    #[tokio::test]
    async fn test_real_dir_replaces_dir_symlink_no_escape() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        let elsewhere = tmp.path().join("elsewhere");

        fs::create_dir_all(&elsewhere).unwrap();
        write_file(&src.join("data/f.txt"), b"payload");
        fs::create_dir_all(&dst).unwrap();
        unix_fs::symlink(&elsewhere, dst.join("data")).unwrap();

        run_sync(&src, &dst).await;

        let meta = fs::symlink_metadata(dst.join("data")).unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "dst/data must be a real directory, not the old symlink"
        );
        assert_eq!(fs::read(dst.join("data/f.txt")).unwrap(), b"payload");
        assert!(
            !elsewhere.join("f.txt").exists(),
            "the copy must not escape dst_root through the symlink"
        );
    }
}

// ---------------------------------------------------------------------------
// HDD flag propagation: plan carries the flag set at preview time.
// ---------------------------------------------------------------------------

/// `SyncPlan.hdd` must reflect whatever was passed to `.with_hdd()` so that
/// the GUI's `post_run` handler can build the execute engine from the plan
/// alone: fixing the regression where HDD mode was detected at preview but
/// silently ignored at run time.
#[tokio::test]
async fn test_plan_carries_hdd_flag() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a.txt"), b"hello");

    let cfg = default_config();

    let plan_ssd = SyncEngine::new(src.clone(), dst.clone(), cfg.clone())
        .with_hdd(false)
        .preview(None, None)
        .await
        .unwrap();
    assert!(!plan_ssd.hdd, "hdd=false must survive into the plan");

    let plan_hdd = SyncEngine::new(src.clone(), dst.clone(), cfg)
        .with_hdd(true)
        .preview(None, None)
        .await
        .unwrap();
    assert!(plan_hdd.hdd, "hdd=true must survive into the plan");
}

/// `SyncPlan.src_root` and `dst_root` must match the paths the engine was
/// constructed with so `post_run` can build the execute engine without re-reading
/// config.
#[tokio::test]
async fn test_plan_carries_src_dst_roots() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a.txt"), b"hello");

    let cfg = default_config();
    let plan = SyncEngine::new(src.clone(), dst.clone(), cfg)
        .preview(None, None)
        .await
        .unwrap();

    assert_eq!(plan.src_root, src);
    assert_eq!(plan.dst_root, dst);
}

// ---------------------------------------------------------------------------
// Cancellation: walking and hashing poll the token between units, and every
// phase must abort with an error rather than hand back partial data: a
// truncated DST tree would read as mass orphans and get deleted.
// ---------------------------------------------------------------------------

/// A token that is already cancelled.
fn cancelled_token() -> (tokio::sync::watch::Sender<bool>, crate::sync::CancelToken) {
    let (tx, rx) = tokio::sync::watch::channel(true);
    (tx, crate::sync::CancelToken::new(Some(rx)))
}

#[test]
fn test_walk_aborts_when_cancelled() {
    use crate::sync::walker;
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("a.txt"), b"a");
    write_file(&src.join("sub/b.txt"), b"b");

    let (_tx, cancel) = cancelled_token();
    let excludes = walker::build_excludes(&[]).unwrap();
    let err = walker::walk(&src, &excludes, "src", None, &cancel)
        .expect_err("a cancelled walk must fail, never return a partial tree");
    assert_eq!(err.to_string(), "cancelled");
}

#[test]
fn test_match_trees_aborts_when_cancelled() {
    use crate::sync::{matcher, walker};
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a.txt"), b"content");
    write_file(&dst.join("b.txt"), b"content");

    let excludes = walker::build_excludes(&[]).unwrap();
    let live = crate::sync::CancelToken::default();
    let src_entries = walker::walk(&src, &excludes, "src", None, &live).unwrap();
    let dst_entries = walker::walk(&dst, &excludes, "dst", None, &live).unwrap();

    let (_tx, cancel) = cancelled_token();
    let result = matcher::match_trees(
        &src_entries,
        &dst_entries,
        None,
        crate::drive::DriveProfile::all_hdd(false),
        &cancel,
    );
    match result {
        Ok(_) => panic!("a cancelled match must fail, never return a partial classification"),
        Err(e) => assert_eq!(e.to_string(), "cancelled"),
    }
}

#[tokio::test]
async fn test_preview_cancelled_returns_status_to_idle() {
    use crate::progress::SyncStatus;

    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a.txt"), b"hello");
    fs::create_dir_all(&dst).unwrap();

    let (tx, _rx) = tokio::sync::broadcast::channel::<ProgressEvent>(64);
    let progress = Arc::new(ProgressState::new(tx));
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(true);

    let engine = SyncEngine::new(src, dst, default_config());
    let err = engine
        .preview(Some(progress.clone()), Some(cancel_rx))
        .await
        .expect_err("preview must fail when cancelled");

    assert_eq!(err.to_string(), "cancelled");
    assert_eq!(
        *progress.status.read().unwrap(),
        SyncStatus::Idle,
        "a cancelled preview must release its Previewing claim"
    );
}

// ---------------------------------------------------------------------------
// Drive profiles: every combination must produce an identical mirror. The
// profile only changes *scheduling* (which side hashes serially, whether
// copies overlap), never the result.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_correct_under_every_drive_profile() {
    use crate::drive::DriveProfile;
    for (src_hdd, dst_hdd) in [(true, true), (true, false), (false, true), (false, false)] {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        // New file, an identical one, a modified one, a move, and an orphan:
        // enough to force hashing on both the SRC and DST side.
        write_file(&src.join("new.txt"), b"brand new");
        write_file(&src.join("same.txt"), b"unchanged");
        write_file(&dst.join("same.txt"), b"unchanged");
        write_file(&src.join("changed.txt"), b"new content");
        write_file(&dst.join("changed.txt"), b"old content!");
        write_file(&src.join("moved/here.bin"), b"movable payload");
        write_file(&dst.join("was_here.bin"), b"movable payload");
        write_file(&dst.join("orphan.txt"), b"delete me");
        set_mtime_to_past(&dst.join("changed.txt"));

        run_sync_with_drives(&src, &dst, DriveProfile { src_hdd, dst_hdd }).await;

        assert_mirror(&src, &dst);
        assert!(
            !dst.join("orphan.txt").exists(),
            "orphan must be deleted (src_hdd={src_hdd}, dst_hdd={dst_hdd})"
        );

        // And the run converges: a second preview plans nothing.
        let engine = SyncEngine::new(src.clone(), dst.clone(), default_config())
            .with_drives(DriveProfile { src_hdd, dst_hdd });
        let plan = engine.preview(None, None).await.unwrap();
        assert!(
            plan.is_noop(),
            "re-preview must be a no-op (src_hdd={src_hdd}, dst_hdd={dst_hdd}): {:?}",
            plan.ops
        );
    }
}

#[test]
fn test_serial_copies_only_when_an_endpoint_is_hdd() {
    use crate::drive::DriveProfile;
    // Copies read SRC and write DST, so either side being spinning media
    // forces them serial; scanning is per-endpoint and never constrained.
    assert!(
        DriveProfile {
            src_hdd: true,
            dst_hdd: true
        }
        .serial_copies()
    );
    assert!(
        DriveProfile {
            src_hdd: true,
            dst_hdd: false
        }
        .serial_copies()
    );
    assert!(
        DriveProfile {
            src_hdd: false,
            dst_hdd: true
        }
        .serial_copies()
    );
    assert!(
        !DriveProfile {
            src_hdd: false,
            dst_hdd: false
        }
        .serial_copies()
    );
    assert!(DriveProfile::all_hdd(true).serial_copies());
    assert!(!DriveProfile::all_hdd(false).serial_copies());
}

// ---------------------------------------------------------------------------
// Type-change collisions and rename edge cases
// (regression tests for the 2026-08-14 review fixes)
// ---------------------------------------------------------------------------

/// Preview after a completed sync must plan zero ops: first-run convergence.
async fn assert_no_ops_on_preview(src: &Path, dst: &Path) {
    let engine = SyncEngine::new(src.to_path_buf(), dst.to_path_buf(), default_config());
    let plan = engine.preview(None, None).await.unwrap();
    assert!(
        plan.is_noop(),
        "expected converged state, but re-preview still plans ops: {:?}",
        plan.ops
    );
}

// SRC has an *empty* directory where DST has a plain file. The MkDir clears
// the file itself, so the planner must suppress the file's orphan Delete:
// otherwise the phase-4 Delete removes the freshly created directory again.
#[tokio::test]
async fn test_type_conflict_empty_dir_over_file() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    fs::create_dir_all(src.join("foo")).unwrap();
    write_file(&dst.join("foo"), b"wrong type");

    run_sync(&src, &dst).await;

    assert!(
        dst.join("foo").is_dir(),
        "dst/foo must be a directory after a single run"
    );
    assert_no_ops_on_preview(&src, &dst).await;
}

// A file-level Move whose target path is currently a DST *directory*: the
// directory's Delete/RmDir ops must be hoisted before the move phase, or the
// rename fails on the first run and a second run is needed.
#[tokio::test]
async fn test_move_onto_path_occupied_by_dst_dir() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    // Previous DST state: a directory `report/` and a file `data.bin`.
    write_file(&dst.join("report/old.txt"), b"old");
    write_file(&dst.join("data.bin"), b"payload-content");
    // SRC now holds data.bin's content at the path `report`: a file where
    // DST has a directory, reached via move detection.
    write_file(&src.join("report"), b"payload-content");

    run_sync(&src, &dst).await;

    assert_eq!(
        fs::read(dst.join("report")).unwrap(),
        b"payload-content",
        "move onto the cleared directory path must succeed in run 1"
    );
    assert!(!dst.join("data.bin").exists());
    assert_no_ops_on_preview(&src, &dst).await;
}

// A DST file occupying a dir-move target while being the *source* of a
// file-level move: the vacating file move must run before the dir rename
// (it has no Delete op the pre-deletion hoist could use).
#[tokio::test]
async fn test_dir_move_target_vacated_by_file_move() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    // Previous DST state: file `docs` and directory `olddocs/`.
    write_file(&dst.join("docs"), b"unique-x-content");
    write_file(&dst.join("olddocs/a.txt"), b"aaaa");
    // SRC: dir renamed olddocs -> docs (identical fingerprint), and the old
    // `docs` file content now lives at a new path.
    write_file(&src.join("docs/a.txt"), b"aaaa");
    write_file(&src.join("notes_x"), b"unique-x-content");

    run_sync(&src, &dst).await;

    assert!(dst.join("docs").is_dir(), "dir rename must land in run 1");
    assert_eq!(fs::read(dst.join("docs/a.txt")).unwrap(), b"aaaa");
    assert_eq!(fs::read(dst.join("notes_x")).unwrap(), b"unique-x-content");
    assert!(!dst.join("olddocs").exists());
    assert_no_ops_on_preview(&src, &dst).await;
}

// A brand-new *empty* directory inside a renamed dir has no Copy to create it
// via create_dir_all: its MkDir must survive the rename-subtree filter (and
// execute after the dir move).
#[tokio::test]
async fn test_new_empty_dir_inside_renamed_dir() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&dst.join("b/f.txt"), b"content");
    write_file(&src.join("a/f.txt"), b"content");
    fs::create_dir_all(src.join("a/empty")).unwrap();

    run_sync(&src, &dst).await;

    assert!(
        dst.join("a/empty").is_dir(),
        "new empty dir inside the renamed dir must exist after run 1"
    );
    assert!(!dst.join("b").exists());
    assert_no_ops_on_preview(&src, &dst).await;
}

// Windows: a case-only dir rename combined with content changes defeats
// fingerprint rename detection; the planner must still repair the stored case
// in run 1 via a synthesized dir-level CaseRename (not a no-op MkDir + RmDir).
#[cfg(windows)]
#[tokio::test]
async fn test_case_only_dir_rename_with_content_change() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");

    write_file(&dst.join("Photos/img.jpg"), b"image-bytes");
    write_file(&src.join("photos/img.jpg"), b"image-bytes");
    write_file(&src.join("photos/new.jpg"), b"new-image");

    run_sync(&src, &dst).await;

    let stored_name = fs::read_dir(&dst)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.eq_ignore_ascii_case("photos"))
        .expect("photos dir must exist");
    assert_eq!(
        stored_name, "photos",
        "stored case must be repaired in run 1 despite the content change"
    );
    assert_eq!(fs::read(dst.join("photos/new.jpg")).unwrap(), b"new-image");
    assert_no_ops_on_preview(&src, &dst).await;
}
