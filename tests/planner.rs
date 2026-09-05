//! Planner tests: `plan()` turns a MatchOutput plus the two directory lists
//! into an ordered op list, so these build the inputs directly and assert on
//! the ops that come out. No filesystem, no timing.

use dirsync::sync::matcher::{MatchOutput, MatchResult, MatchedEntry, OrphanEntry, RenamedDir};
use dirsync::sync::planner::{SyncOp, SyncPlan, plan};
use dirsync::sync::walker::FileEntry;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SRC_ROOT: &str = "/src";
const DST_ROOT: &str = "/dst";

fn file(rel: &str, size: u64) -> FileEntry {
    FileEntry {
        rel_path: PathBuf::from(rel),
        abs_path: PathBuf::from(SRC_ROOT).join(rel),
        size,
        mtime: SystemTime::UNIX_EPOCH,
        is_dir: false,
        symlink_target: None,
    }
}

fn dst_file(rel: &str, size: u64) -> FileEntry {
    FileEntry {
        abs_path: PathBuf::from(DST_ROOT).join(rel),
        ..file(rel, size)
    }
}

fn symlink(rel: &str, target: &str) -> FileEntry {
    FileEntry {
        symlink_target: Some(PathBuf::from(target)),
        ..file(rel, 0)
    }
}

fn dir(rel: &str) -> FileEntry {
    FileEntry {
        is_dir: true,
        ..file(rel, 0)
    }
}

fn matched(src: FileEntry, result: MatchResult) -> MatchedEntry {
    MatchedEntry {
        src,
        result,
        src_hash: None,
        case_renamed_from: None,
    }
}

fn output(matched: Vec<MatchedEntry>, orphans: Vec<FileEntry>) -> MatchOutput {
    MatchOutput {
        matched,
        orphans: orphans.into_iter().map(|dst| OrphanEntry { dst }).collect(),
        renamed_dirs: vec![],
    }
}

fn run(out: MatchOutput, src_dirs: &[FileEntry], dst_dirs: &[FileEntry]) -> SyncPlan {
    plan(
        out,
        src_dirs,
        dst_dirs,
        Path::new(SRC_ROOT),
        Path::new(DST_ROOT),
        false,
    )
}

fn dst(rel: &str) -> PathBuf {
    PathBuf::from(DST_ROOT).join(rel)
}

// --- Symlink ops ---

#[test]
fn a_new_src_symlink_becomes_a_symlink_op_not_a_copy() {
    let out = output(
        vec![matched(
            symlink("link", "target.txt"),
            MatchResult::NewInSrc,
        )],
        vec![],
    );

    let plan = run(out, &[], &[]);

    match plan.ops.as_slice() {
        [SyncOp::Symlink { target, dst: d }] => {
            assert_eq!(target, Path::new("target.txt"));
            assert_eq!(*d, dst("link"));
        }
        other => panic!("expected one Symlink op, got {other:?}"),
    }
    assert_eq!(plan.symlink_count, 1);
    assert_eq!(plan.copy_count, 0);
}

#[test]
fn a_retargeted_symlink_becomes_a_symlink_op_not_an_overwrite() {
    // SamePathDifferentContent is what the matcher reports both for a changed
    // link target and for a link landing on a regular DST file. Either way the
    // executor has to recreate the link, never write bytes through the path.
    let out = output(
        vec![matched(
            symlink("link", "new-target.txt"),
            MatchResult::SamePathDifferentContent,
        )],
        vec![],
    );

    let plan = run(out, &[], &[]);

    assert!(matches!(plan.ops.as_slice(), [SyncOp::Symlink { .. }]));
    assert_eq!(plan.overwrite_count, 0);
}

#[test]
fn regular_files_still_produce_copy_and_overwrite_ops() {
    let out = output(
        vec![
            matched(file("new.txt", 10), MatchResult::NewInSrc),
            matched(
                file("changed.txt", 20),
                MatchResult::SamePathDifferentContent,
            ),
            matched(file("same.txt", 30), MatchResult::Identical),
            matched(file("stale.txt", 40), MatchResult::IdenticalMtimeDiverged),
        ],
        vec![],
    );

    let plan = run(out, &[], &[]);

    assert_eq!(plan.copy_count, 1);
    assert_eq!(plan.overwrite_count, 1);
    assert_eq!(plan.touch_count, 1);
    // An identical file produces no op at all, only a count.
    assert_eq!(plan.identical_count, 1);
    assert_eq!(plan.ops.len(), 3);
}

// --- Orphans and renamed directories ---

#[test]
fn an_orphan_whose_path_a_write_op_claims_is_not_deleted() {
    // SRC has a file at the path DST holds as an orphan: the Copy overwrites
    // it. Emitting a Delete as well would race the copy for the same path.
    let out = output(
        vec![matched(file("taken.txt", 5), MatchResult::NewInSrc)],
        vec![dst_file("taken.txt", 99)],
    );

    let plan = run(out, &[], &[]);

    assert_eq!(plan.delete_count, 0);
    assert!(matches!(plan.ops.as_slice(), [SyncOp::Copy { .. }]));
}

#[test]
fn a_true_orphan_is_deleted() {
    let out = output(vec![], vec![dst_file("gone.txt", 7)]);

    let plan = run(out, &[], &[]);

    match plan.ops.as_slice() {
        [SyncOp::Delete { path, size }] => {
            assert_eq!(*path, dst("gone.txt"));
            assert_eq!(*size, 7);
        }
        other => panic!("expected one Delete, got {other:?}"),
    }
}

#[test]
fn an_orphan_inside_a_renamed_directory_is_deleted_at_its_post_rename_path() {
    // The dir Move runs before the deletes, so by then the file lives under
    // the new name: deleting the pre-rename path would fail.
    let mut out = output(vec![], vec![dst_file("old/sub/gone.txt", 3)]);
    out.renamed_dirs = vec![RenamedDir {
        src_rel: PathBuf::from("new"),
        dst_rel: PathBuf::from("old"),
    }];

    let plan = run(
        out,
        &[dir("new"), dir("new/sub")],
        &[dir("old"), dir("old/sub")],
    );

    let deletes: Vec<&PathBuf> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            SyncOp::Delete { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(deletes, vec![&dst("new/sub/gone.txt")]);
}

#[test]
fn a_renamed_directory_becomes_a_single_move_op() {
    let mut out = output(vec![], vec![]);
    out.renamed_dirs = vec![RenamedDir {
        src_rel: PathBuf::from("new"),
        dst_rel: PathBuf::from("old"),
    }];

    let plan = run(out, &[dir("new")], &[dir("old")]);

    match plan.ops.as_slice() {
        [SyncOp::Move { from, to, is_dir }] => {
            assert_eq!(*from, dst("old"));
            assert_eq!(*to, dst("new"));
            assert!(is_dir);
        }
        other => panic!("expected one dir Move, got {other:?}"),
    }
}

#[test]
fn a_src_directory_missing_from_dst_gets_a_mkdir() {
    let plan = run(output(vec![], vec![]), &[dir("fresh")], &[]);

    assert!(
        matches!(plan.ops.as_slice(), [SyncOp::MkDir { path }] if *path == dst("fresh")),
        "got {:?}",
        plan.ops
    );
}

#[test]
fn a_dst_directory_with_no_src_counterpart_is_removed() {
    let plan = run(output(vec![], vec![]), &[], &[dir("obsolete")]);

    assert!(
        matches!(plan.ops.as_slice(), [SyncOp::RmDir { path }] if *path == dst("obsolete")),
        "got {:?}",
        plan.ops
    );
}

// --- Case-only renames (Windows) ---
//
// NTFS compares names case-insensitively, so "Docs" and "docs" are the same
// directory: a MkDir would no-op into the existing one and the paired RmDir
// would fail non-empty, leaving the old spelling in place. The planner emits
// a CaseRename instead, which the executor performs as a two-step rename.

#[test]
#[cfg(windows)]
fn a_file_move_that_only_changes_case_becomes_a_case_rename() {
    let out = output(
        vec![matched(
            file("Readme.md", 12),
            MatchResult::MovedFrom(PathBuf::from("readme.md")),
        )],
        vec![],
    );

    let plan = run(out, &[], &[]);

    match plan.ops.as_slice() {
        [SyncOp::CaseRename { from, to, is_dir }] => {
            assert_eq!(*from, dst("readme.md"));
            assert_eq!(*to, dst("Readme.md"));
            assert!(!is_dir);
        }
        other => panic!("expected a file CaseRename, got {other:?}"),
    }
}

#[test]
#[cfg(windows)]
fn a_directory_rename_that_only_changes_case_becomes_a_case_rename() {
    let mut out = output(vec![], vec![]);
    out.renamed_dirs = vec![RenamedDir {
        src_rel: PathBuf::from("Docs"),
        dst_rel: PathBuf::from("docs"),
    }];

    let plan = run(out, &[dir("Docs")], &[dir("docs")]);

    match plan.ops.as_slice() {
        [SyncOp::CaseRename { from, to, is_dir }] => {
            assert_eq!(*from, dst("docs"));
            assert_eq!(*to, dst("Docs"));
            assert!(is_dir);
        }
        other => panic!("expected a dir CaseRename, got {other:?}"),
    }
}

#[test]
#[cfg(windows)]
fn a_new_dir_matching_an_extra_dst_dir_by_case_is_repaired_in_place() {
    // No fingerprint rename here: the contents differ, so the dir arrives as
    // a new SRC dir plus an extra DST dir. Without the CaseRename this pair
    // would become a MkDir that no-ops and an RmDir that fails.
    let plan = run(output(vec![], vec![]), &[dir("Docs")], &[dir("docs")]);

    assert!(
        matches!(
            plan.ops.as_slice(),
            [SyncOp::CaseRename { from, to, is_dir: true }] if *from == dst("docs") && *to == dst("Docs")
        ),
        "got {:?}",
        plan.ops
    );
    // The old spelling must not also be removed: that would delete the
    // directory the CaseRename just repaired.
    assert_eq!(plan.delete_count, 0);
    assert!(!plan.ops.iter().any(|op| matches!(op, SyncOp::RmDir { .. })));
}

#[test]
#[cfg(windows)]
fn a_case_renamed_entry_is_not_also_counted_as_identical() {
    let mut entry = matched(file("Notes.txt", 4), MatchResult::Identical);
    entry.case_renamed_from = Some(PathBuf::from("notes.txt"));

    let plan = run(output(vec![entry], vec![]), &[], &[]);

    // The CaseRename is the whole job; counting the file as identical too
    // reported it twice in the summary.
    assert_eq!(plan.identical_count, 0);
    assert!(matches!(
        plan.ops.as_slice(),
        [SyncOp::CaseRename { is_dir: false, .. }]
    ));
}

#[test]
#[cfg(not(windows))]
fn a_move_that_only_changes_case_is_a_plain_move_off_windows() {
    let out = output(
        vec![matched(
            file("Readme.md", 12),
            MatchResult::MovedFrom(PathBuf::from("readme.md")),
        )],
        vec![],
    );

    let plan = run(out, &[], &[]);

    assert!(matches!(
        plan.ops.as_slice(),
        [SyncOp::Move { is_dir: false, .. }]
    ));
}

// --- Skipping directories from the GUI ---

#[test]
fn skipping_a_directory_drops_its_writes_but_keeps_its_cleanup() {
    let out = output(
        vec![
            matched(file("skipme/a.txt", 1), MatchResult::NewInSrc),
            matched(file("skipme/link", 0), MatchResult::NewInSrc),
            matched(
                file("skipme/stale.txt", 2),
                MatchResult::IdenticalMtimeDiverged,
            ),
            matched(file("keep/b.txt", 3), MatchResult::NewInSrc),
        ],
        vec![dst_file("skipme/orphan.txt", 4)],
    );

    let plan = run(out, &[dir("skipme/fresh")], &[]).without_skipped(&["skipme".to_string()]);

    // Writes under the skipped prefix are gone.
    let skipped = dst("skipme");
    assert!(!plan.ops.iter().any(|op| matches!(op,
        SyncOp::Copy { dst: d, .. } | SyncOp::TouchMtime { dst: d, .. } if d.starts_with(&skipped)
    )));
    assert!(!plan.ops.iter().any(|op| matches!(op,
        SyncOp::MkDir { path } if path.starts_with(&skipped)
    )));
    // Cleanup is not a write: skipping a source directory suppresses copies
    // into DST, it does not cancel deletion of DST orphans.
    assert_eq!(plan.delete_count, 1);
    // The sibling survives untouched.
    let kept = dst("keep/b.txt");
    assert!(plan.ops.iter().any(|op| matches!(op,
        SyncOp::Copy { dst: d, .. } if *d == kept
    )));
}
