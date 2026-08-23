use super::matcher::{MatchOutput, MatchResult, RenamedDir};
use super::walker::FileEntry;
use crate::fmt::fmt_bytes;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Virtual byte weight credited to every non-Copy/Overwrite op so that moves,
/// deletes, mkdirs, etc. advance the overall progress bar.
pub(super) const OP_TOKEN_BYTES: u64 = 8 * 1024;

#[derive(Debug, Clone, serde::Serialize)]
pub enum SyncOp {
    /// Copy a new file from SRC to DST.
    Copy {
        src: PathBuf,
        dst: PathBuf,
        size: u64,
        hash: Option<[u8; 32]>,
    },
    /// Overwrite an existing DST file with SRC content.
    Overwrite {
        src: PathBuf,
        dst: PathBuf,
        size: u64,
        hash: Option<[u8; 32]>,
    },
    /// Move/rename a file or directory within DST (no read from SRC needed).
    Move {
        from: PathBuf,
        to: PathBuf,
        is_dir: bool,
    },
    /// Delete an orphan file in DST.
    Delete { path: PathBuf, size: u64 },
    /// Create a directory in DST.
    MkDir { path: PathBuf },
    /// Remove an empty directory in DST.
    RmDir { path: PathBuf },
    /// File content is identical but DST mtime diverged from SRC: fix mtime only.
    TouchMtime { src: PathBuf, dst: PathBuf },
    /// Create (or replace) a symlink at dst with the given target.
    Symlink { target: PathBuf, dst: PathBuf },
    /// Rename a DST entry whose path differs from the SRC path only in case.
    /// On Windows (case-insensitive NTFS) a straight rename is a no-op; the
    /// executor uses a two-step temp-rename to force the directory-entry update.
    #[cfg(windows)]
    CaseRename {
        from: PathBuf,
        to: PathBuf,
        is_dir: bool,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncPlan {
    pub ops: Vec<SyncOp>,
    pub total_bytes: u64,
    pub copy_count: usize,
    pub move_count: usize,
    pub delete_count: usize,
    pub overwrite_count: usize,
    pub identical_count: usize,
    pub touch_count: usize,
    pub symlink_count: usize,
    pub src_root: PathBuf,
    pub dst_root: PathBuf,
    /// Whether HDD-safe (serial) I/O was detected at preview time.
    /// Carried on the plan so `post_run` can build the execute engine without
    /// re-reading config or re-detecting the drive type.
    pub hdd: bool,
    /// Aggregate size (bytes) of all SRC files under each directory, keyed by
    /// forward-slash relative path. Used by the GUI to show directory sizes.
    pub src_dir_sizes: HashMap<String, u64>,
    /// Write targets (absolute) that DST currently holds as a *directory*,
    /// per the preview's DST walk. The executor clears these by hoisting
    /// their Delete/RmDir ops before any write phase; flagging them here
    /// costs zero I/O, where the executor used to issue one serial stat per
    /// Copy/Overwrite/Move/Symlink op on the async runtime before execution
    /// started. Preview-time data, like the rest of the plan.
    pub dir_blocked_targets: Vec<PathBuf>,
}

impl SyncPlan {
    /// Drop every op that writes at or below one of `skip_prefixes` (paths
    /// relative to `dst_root`, forward-slashed), recomputing counts and totals.
    ///
    /// `Delete` and `RmDir` are deliberately kept: skipping a source directory
    /// suppresses writes into DST, it does not cancel cleanup of DST orphans.
    /// This mirrors the frontend's own display filter, which also keeps deletes.
    ///
    /// Without this the GUI's "Skip this directory" was cosmetic: it filtered
    /// the displayed op list while `post_run` executed the unmodified plan.
    pub fn without_skipped(mut self, skip_prefixes: &[String]) -> SyncPlan {
        let prefixes: Vec<String> = skip_prefixes
            .iter()
            .map(|p| p.replace('\\', "/").trim_end_matches('/').to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        if prefixes.is_empty() {
            return self;
        }

        let is_skipped = |rel: &str| {
            prefixes
                .iter()
                .any(|p| rel == p || rel.starts_with(&format!("{p}/")))
        };

        // Move dst_root out for the duration: `retain` takes `&mut self.ops`,
        // so the predicate cannot also hold a `&self` to read it. Restored below.
        let dst_root = std::mem::take(&mut self.dst_root);
        // Path relative to dst_root, forward-slashed: the same form the GUI
        // displays and therefore the same form its skip prefixes arrive in.
        let rel_to_dst = |p: &Path| -> Option<String> {
            p.strip_prefix(&dst_root).ok().map(crate::paths::to_slash)
        };

        self.ops.retain(|op| {
            let target = match op {
                // Cleanup ops are never skipped.
                SyncOp::Delete { .. } | SyncOp::RmDir { .. } => return true,
                SyncOp::Copy { dst, .. }
                | SyncOp::Overwrite { dst, .. }
                | SyncOp::Symlink { dst, .. }
                | SyncOp::TouchMtime { dst, .. }
                | SyncOp::MkDir { path: dst } => dst,
                SyncOp::Move { to, .. } => to,
                #[cfg(windows)]
                SyncOp::CaseRename { to, .. } => to,
            };
            match rel_to_dst(target) {
                Some(rel) => !is_skipped(&rel),
                None => true,
            }
        });

        self.dst_root = dst_root;
        self.recount();
        self
    }

    /// Recompute all derived counters from `ops`. `identical_count` is not
    /// derived from ops (identical files produce none) and is left untouched.
    fn recount(&mut self) {
        self.copy_count = 0;
        self.move_count = 0;
        self.delete_count = 0;
        self.overwrite_count = 0;
        self.touch_count = 0;
        self.symlink_count = 0;
        let mut copy_bytes = 0u64;

        for op in &self.ops {
            match op {
                SyncOp::Copy { size, .. } => {
                    self.copy_count += 1;
                    copy_bytes += size;
                }
                SyncOp::Overwrite { size, .. } => {
                    self.overwrite_count += 1;
                    copy_bytes += size;
                }
                SyncOp::Move { .. } => self.move_count += 1,
                SyncOp::Delete { .. } => self.delete_count += 1,
                SyncOp::TouchMtime { .. } => self.touch_count += 1,
                SyncOp::Symlink { .. } => self.symlink_count += 1,
                SyncOp::MkDir { .. } | SyncOp::RmDir { .. } => {}
                #[cfg(windows)]
                SyncOp::CaseRename { .. } => self.move_count += 1,
            }
        }

        let non_copy_ops = self
            .ops
            .len()
            .saturating_sub(self.copy_count + self.overwrite_count);
        self.total_bytes = copy_bytes + non_copy_ops as u64 * OP_TOKEN_BYTES;
    }

    /// True when executing this plan would change nothing on disk. The one
    /// no-op predicate shared by the CLI and the GUI log message: counting
    /// selected op categories instead misses TouchMtime/MkDir/RmDir-only plans.
    pub fn is_noop(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn summary(&self) -> String {
        use crate::fmt::fmt_count;
        format!(
            "copy={} overwrite={} move={} delete={} symlink={} identical={} touch={} ({} to transfer)",
            fmt_count(self.copy_count),
            fmt_count(self.overwrite_count),
            fmt_count(self.move_count),
            fmt_count(self.delete_count),
            fmt_count(self.symlink_count),
            fmt_count(self.identical_count),
            fmt_count(self.touch_count),
            fmt_bytes(self.total_bytes),
        )
    }
}

/// True when `from` and `to` are in the same directory and differ only in case.
/// Used on Windows to detect renames that a straight `fs::rename` won't fix.
#[cfg(windows)]
fn is_case_only_rename(from: &std::path::Path, to: &std::path::Path) -> bool {
    from != to
        && from.parent() == to.parent()
        && from.as_os_str().to_string_lossy().to_lowercase()
            == to.as_os_str().to_string_lossy().to_lowercase()
}

pub fn plan(
    match_output: MatchOutput,
    src_dirs: &[FileEntry],
    dst_dirs: &[FileEntry],
    src_root: &Path,
    dst_root: &Path,
    hdd: bool,
) -> SyncPlan {
    let mut ops: Vec<SyncOp> = Vec::new();
    // Every other counter and total_bytes are derived from `ops` by
    // recount() at the end. Identical files produce no op, so their count
    // is the only one accumulated here.
    let mut identical_count = 0;

    let renamed_dirs = &match_output.renamed_dirs;

    let src_dir_paths: HashSet<&PathBuf> = src_dirs.iter().map(|e| &e.rel_path).collect();
    let dst_dir_paths: HashSet<&PathBuf> = dst_dirs.iter().map(|e| &e.rel_path).collect();

    // Indexes for O(path_depth) renamed-dir lookups instead of O(renamed_dirs) linear scans.
    // Keys are owned PathBufs so lookups can use &Path (via PathBuf: Borrow<Path>).
    let src_rename_index: HashMap<PathBuf, &RenamedDir> = renamed_dirs
        .iter()
        .map(|r| (r.src_rel.clone(), r))
        .collect();
    let dst_rename_index: HashMap<PathBuf, &RenamedDir> = renamed_dirs
        .iter()
        .map(|r| (r.dst_rel.clone(), r))
        .collect();

    // ------------------------------------------------------------------
    // MkDir: SRC dirs not present in DST, excluding the renamed-dir tree.
    // The dir-level Move op creates the root; existing subdirs are moved
    // along; brand-new subdirs holding files are created by create_dir_all
    // inside Copy. A brand-new *empty* dir inside a renamed subtree has no
    // Copy to create it, so it keeps its MkDir (at the post-rename path):
    // the executor sequences those after the dir move so create_dir_all
    // can't materialize the rename target early.
    // ------------------------------------------------------------------
    let mut dirs_to_create: Vec<&PathBuf> = src_dir_paths
        .iter()
        .filter(|p| !dst_dir_paths.contains(*p))
        .filter(|p| match find_in_rename_index(p, &src_rename_index) {
            None => true,
            Some(rename) => match p.strip_prefix(&rename.src_rel) {
                Ok(suffix) => {
                    !suffix.as_os_str().is_empty()
                        && !dst_dir_paths.contains(&rename.dst_rel.join(suffix))
                }
                Err(_) => true,
            },
        })
        .copied()
        .collect();
    dirs_to_create.sort();

    // Windows: a new SRC dir whose name matches an extra DST dir except for
    // letter case is repaired with a dir-level CaseRename instead of the
    // MkDir + RmDir pair: on NTFS that MkDir silently no-ops into the
    // existing dir and the RmDir silently fails non-empty, leaving the old
    // case in place whenever the dir's contents changed alongside the rename
    // (identical contents take the fingerprint-rename path instead).
    #[cfg(windows)]
    let extra_dst_dirs_lower: HashMap<String, &PathBuf> = dst_dir_paths
        .iter()
        .filter(|d| !src_dir_paths.contains(*d))
        .filter(|d| find_in_rename_index(d, &dst_rename_index).is_none())
        .map(|d| (d.to_string_lossy().to_lowercase(), *d))
        .collect();
    #[cfg(windows)]
    let mut case_renamed_dst_dirs: HashSet<&PathBuf> = HashSet::new();

    for dir in dirs_to_create {
        #[cfg(windows)]
        if let Some(old) = extra_dst_dirs_lower.get(&dir.to_string_lossy().to_lowercase()) {
            ops.push(SyncOp::CaseRename {
                from: dst_root.join(old),
                to: dst_root.join(dir),
                is_dir: true,
            });
            case_renamed_dst_dirs.insert(*old);
            continue;
        }
        ops.push(SyncOp::MkDir {
            path: dst_root.join(dir),
        });
    }

    // ------------------------------------------------------------------
    // Move: directory-level renames first (single OS rename, very fast).
    // ------------------------------------------------------------------
    for rename in renamed_dirs {
        let from = dst_root.join(&rename.dst_rel);
        let to = dst_root.join(&rename.src_rel);
        #[cfg(windows)]
        if is_case_only_rename(&from, &to) {
            ops.push(SyncOp::CaseRename {
                from,
                to,
                is_dir: true,
            });
            continue;
        }
        ops.push(SyncOp::Move {
            from,
            to,
            is_dir: true,
        });
    }

    // ------------------------------------------------------------------
    // Move: file-level renames detected by the matcher.
    // ------------------------------------------------------------------
    for entry in &match_output.matched {
        if let MatchResult::MovedFrom(old_dst_rel) = &entry.result {
            let from = dst_root.join(old_dst_rel);
            let to = dst_root.join(&entry.src.rel_path);
            #[cfg(windows)]
            if is_case_only_rename(&from, &to) {
                ops.push(SyncOp::CaseRename {
                    from,
                    to,
                    is_dir: false,
                });
                continue;
            }
            ops.push(SyncOp::Move {
                from,
                to,
                is_dir: false,
            });
        }
    }

    // ------------------------------------------------------------------
    // CaseRename: case-insensitive path matches on Windows.
    // Emitted before Copy/Overwrite so the correct name is in place when
    // content is written. Identical/IdenticalMtimeDiverged entries get a
    // CaseRename too: the content op (or lack thereof) is handled below.
    // ------------------------------------------------------------------
    #[cfg(windows)]
    for entry in &match_output.matched {
        if let Some(old_rel) = &entry.case_renamed_from {
            let from = dst_root.join(old_rel);
            let to = dst_root.join(&entry.src.rel_path);
            ops.push(SyncOp::CaseRename {
                from,
                to,
                is_dir: false,
            });
        }
    }

    // ------------------------------------------------------------------
    // Copy / Overwrite / Symlink
    // ------------------------------------------------------------------
    for entry in &match_output.matched {
        match &entry.result {
            MatchResult::NewInSrc | MatchResult::SamePathDifferentContent => {
                let dst = dst_root.join(&entry.src.rel_path);
                if let Some(target) = &entry.src.symlink_target {
                    ops.push(SyncOp::Symlink {
                        target: target.clone(),
                        dst,
                    });
                } else if matches!(entry.result, MatchResult::NewInSrc) {
                    ops.push(SyncOp::Copy {
                        src: entry.src.abs_path.clone(),
                        dst,
                        size: entry.src.size,
                        hash: entry.src_hash,
                    });
                } else {
                    ops.push(SyncOp::Overwrite {
                        src: entry.src.abs_path.clone(),
                        dst,
                        size: entry.src.size,
                        hash: entry.src_hash,
                    });
                }
            }
            MatchResult::Identical => {
                // A case-renamed entry already counted as a move above;
                // counting it again here reported the same file twice.
                #[cfg(windows)]
                if entry.case_renamed_from.is_some() {
                    continue;
                }
                identical_count += 1;
            }
            MatchResult::IdenticalMtimeDiverged => {
                ops.push(SyncOp::TouchMtime {
                    src: entry.src.abs_path.clone(),
                    dst: dst_root.join(&entry.src.rel_path),
                });
            }
            MatchResult::MovedFrom(_) => {}
        }
    }

    // ------------------------------------------------------------------
    // Delete orphan files.
    // Files inside a renamed dir are now at the *new* path (after the
    // dir Move op), so we compute the post-rename absolute path.
    //
    // Suppress deletes whose target is also the destination of a file-level
    // Move op: the rename atomically replaces whatever was at that path, so a
    // separate Delete would remove the correctly-moved file.
    // ------------------------------------------------------------------
    // Paths already targeted by a write op (move, copy, overwrite, symlink,
    // mkdir). Orphan deletes for these paths are suppressed: the write op
    // already replaces whatever was there, so a separate delete would destroy
    // the result. This handles type-mismatch cases (e.g. DST symlink where
    // SRC has a regular file, or vice versa). MkDir targets count as writes
    // too: the executor clears a non-directory occupant itself, and a
    // surviving Delete would fire against the freshly created directory.
    let occupied_dsts: HashSet<PathBuf> = ops
        .iter()
        .filter_map(|op| match op {
            SyncOp::Move {
                to, is_dir: false, ..
            } => Some(to.clone()),
            SyncOp::Copy { dst, .. } => Some(dst.clone()),
            SyncOp::Overwrite { dst, .. } => Some(dst.clone()),
            SyncOp::Symlink { dst, .. } => Some(dst.clone()),
            SyncOp::MkDir { path } => Some(path.clone()),
            _ => None,
        })
        .collect();

    for orphan in &match_output.orphans {
        let path = adjusted_path(&orphan.dst, &dst_rename_index, dst_root);
        if !occupied_dsts.contains(&path) {
            ops.push(SyncOp::Delete {
                path,
                size: orphan.dst.size,
            });
        }
    }

    // ------------------------------------------------------------------
    // RmDir: DST dirs no longer needed, deepest first.
    // For dirs inside a renamed dir we use the post-rename (new) path.
    // ------------------------------------------------------------------
    let mut dirs_to_remove: Vec<PathBuf> = Vec::new();
    for dst_p in &dst_dir_paths {
        if let Some(rename) = find_in_rename_index(dst_p, &dst_rename_index) {
            if *dst_p == &rename.dst_rel {
                continue; // renamed dir itself - Move handles it
            }
            if let Ok(suffix) = dst_p.strip_prefix(&rename.dst_rel) {
                let new_rel = rename.src_rel.join(suffix);
                if !src_dir_paths.contains(&new_rel) {
                    dirs_to_remove.push(dst_root.join(&new_rel));
                }
            }
        } else if !src_dir_paths.contains(dst_p) {
            // Repaired in place by a synthesized dir-level CaseRename above:
            // removing it would delete the renamed directory.
            #[cfg(windows)]
            if case_renamed_dst_dirs.contains(dst_p) {
                continue;
            }
            dirs_to_remove.push(dst_root.join(dst_p));
        }
    }
    dirs_to_remove.sort_by(|a, b| b.cmp(a)); // deepest first
    for path in dirs_to_remove {
        ops.push(SyncOp::RmDir { path });
    }

    // Write targets that DST currently holds as a directory (see the field
    // doc on SyncPlan). Derived from the walk data already in hand.
    let dir_blocked_targets: Vec<PathBuf> = ops
        .iter()
        .filter_map(|op| match op {
            SyncOp::Copy { dst, .. }
            | SyncOp::Overwrite { dst, .. }
            | SyncOp::Symlink { dst, .. } => Some(dst),
            SyncOp::Move {
                to, is_dir: false, ..
            } => Some(to),
            _ => None,
        })
        .filter(|abs| {
            abs.strip_prefix(dst_root)
                .is_ok_and(|rel| dst_dir_paths.contains(&rel.to_path_buf()))
        })
        .cloned()
        .collect();

    // Aggregate SRC file sizes per directory for GUI display.
    let mut src_dir_sizes: HashMap<String, u64> = HashMap::new();
    for entry in &match_output.matched {
        let rel = crate::paths::to_slash(&entry.src.rel_path);
        let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        for i in 1..parts.len() {
            *src_dir_sizes.entry(parts[..i].join("/")).or_default() += entry.src.size;
        }
    }

    let mut plan = SyncPlan {
        ops,
        total_bytes: 0,
        copy_count: 0,
        move_count: 0,
        delete_count: 0,
        overwrite_count: 0,
        identical_count,
        touch_count: 0,
        symlink_count: 0,
        src_root: src_root.to_path_buf(),
        dst_root: dst_root.to_path_buf(),
        hdd,
        src_dir_sizes,
        dir_blocked_targets,
    };
    plan.recount();
    plan
}

/// Walk ancestors of `rel` (deepest first) and return the first `RenamedDir`
/// whose key equals an ancestor-or-self of `rel`. O(path_depth) with the index.
fn find_in_rename_index<'a>(
    rel: &Path,
    index: &HashMap<PathBuf, &'a RenamedDir>,
) -> Option<&'a RenamedDir> {
    let mut cur = rel;
    loop {
        if let Some(&r) = index.get(cur) {
            return Some(r);
        }
        match cur.parent() {
            Some(p) if !p.as_os_str().is_empty() => cur = p,
            _ => return None,
        }
    }
}

/// Return the correct DST path for an entry, accounting for any directory
/// rename that relocated its parent after the walk.
fn adjusted_path(
    entry: &FileEntry,
    dst_rename_index: &HashMap<PathBuf, &RenamedDir>,
    dst_root: &Path,
) -> PathBuf {
    let mut cur = entry.rel_path.as_path();
    loop {
        if let Some(rename) = dst_rename_index.get(cur) {
            let suffix = entry.rel_path.strip_prefix(cur).unwrap();
            return dst_root.join(&rename.src_rel).join(suffix);
        }
        match cur.parent() {
            Some(p) if !p.as_os_str().is_empty() => cur = p,
            _ => return entry.abs_path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn bare_plan(
        total_bytes: u64,
        copy_count: usize,
        overwrite_count: usize,
        move_count: usize,
        delete_count: usize,
        symlink_count: usize,
        identical_count: usize,
        touch_count: usize,
    ) -> SyncPlan {
        SyncPlan {
            ops: vec![],
            total_bytes,
            copy_count,
            move_count,
            delete_count,
            overwrite_count,
            identical_count,
            touch_count,
            symlink_count,
            src_root: PathBuf::from("/src"),
            dst_root: PathBuf::from("/dst"),
            hdd: false,
            src_dir_sizes: HashMap::new(),
            dir_blocked_targets: vec![],
        }
    }

    // fmt_bytes tests live in crate::fmt alongside the shared implementation.

    // --- SyncPlan::summary ---

    #[test]
    fn test_summary_contains_all_counts() {
        let plan = bare_plan(0, 1, 2, 3, 4, 5, 6, 7);
        let s = plan.summary();
        assert!(s.contains("copy=1"), "summary: {s}");
        assert!(s.contains("overwrite=2"), "summary: {s}");
        assert!(s.contains("move=3"), "summary: {s}");
        assert!(s.contains("delete=4"), "summary: {s}");
        assert!(s.contains("symlink=5"), "summary: {s}");
        assert!(s.contains("identical=6"), "summary: {s}");
        assert!(s.contains("touch=7"), "summary: {s}");
    }

    #[test]
    fn test_summary_includes_human_readable_bytes() {
        let plan = bare_plan(5 * 1024 * 1024, 1, 0, 0, 0, 0, 0, 0);
        let s = plan.summary();
        assert!(s.contains("MB"), "summary should show MB scale: {s}");
    }

    // --- without_skipped ---

    fn plan_with_ops(ops: Vec<SyncOp>) -> SyncPlan {
        let mut p = bare_plan(0, 0, 0, 0, 0, 0, 0, 0);
        p.dst_root = PathBuf::from("/dst");
        p.ops = ops;
        p.recount();
        p
    }

    fn copy_op(rel: &str, size: u64) -> SyncOp {
        SyncOp::Copy {
            src: PathBuf::from("/src").join(rel),
            dst: PathBuf::from("/dst").join(rel),
            size,
            hash: None,
        }
    }

    #[test]
    fn test_without_skipped_drops_copies_under_prefix() {
        let plan = plan_with_ops(vec![
            copy_op("keep/a.txt", 10),
            copy_op("photos/b.txt", 20),
            copy_op("photos/nested/c.txt", 30),
        ]);
        let out = plan.without_skipped(&["photos".to_owned()]);

        assert_eq!(out.ops.len(), 1);
        assert_eq!(out.copy_count, 1);
        let rels: Vec<String> = out
            .ops
            .iter()
            .filter_map(|o| match o {
                SyncOp::Copy { dst, .. } => Some(
                    dst.strip_prefix(&out.dst_root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(rels, vec!["keep/a.txt"]);
    }

    /// A sibling whose name merely starts with the skipped name must survive.
    #[test]
    fn test_without_skipped_does_not_match_name_prefix() {
        let plan = plan_with_ops(vec![
            copy_op("photos2/a.txt", 10),
            copy_op("photos/b.txt", 20),
        ]);
        let out = plan.without_skipped(&["photos".to_owned()]);
        assert_eq!(out.ops.len(), 1);
        assert_eq!(out.copy_count, 1);
    }

    #[test]
    fn test_without_skipped_keeps_deletes_and_rmdirs() {
        let plan = plan_with_ops(vec![
            copy_op("photos/b.txt", 20),
            SyncOp::Delete {
                path: PathBuf::from("/dst/photos/old.txt"),
                size: 5,
            },
            SyncOp::RmDir {
                path: PathBuf::from("/dst/photos/gone"),
            },
        ]);
        let out = plan.without_skipped(&["photos".to_owned()]);

        assert_eq!(out.ops.len(), 2, "cleanup ops must survive a skip");
        assert_eq!(out.copy_count, 0);
        assert_eq!(out.delete_count, 1);
    }

    #[test]
    fn test_without_skipped_recomputes_bytes() {
        let plan = plan_with_ops(vec![
            copy_op("keep/a.txt", 1000),
            copy_op("skip/b.txt", 9000),
        ]);
        assert_eq!(plan.total_bytes, 10_000);

        let out = plan.without_skipped(&["skip".to_owned()]);
        assert_eq!(out.total_bytes, 1000, "skipped bytes must leave the total");
    }

    #[test]
    fn test_without_skipped_accepts_backslashes_and_trailing_slash() {
        let plan = plan_with_ops(vec![copy_op("photos/nested/c.txt", 30)]);
        let out = plan.without_skipped(&["photos\\nested/".to_owned()]);
        assert!(out.ops.is_empty());
    }

    #[test]
    fn test_without_skipped_empty_list_is_identity() {
        let plan = plan_with_ops(vec![copy_op("a.txt", 10), copy_op("b.txt", 20)]);
        let out = plan.without_skipped(&[]);
        assert_eq!(out.ops.len(), 2);
        assert_eq!(out.copy_count, 2);
    }

    #[test]
    fn test_without_skipped_drops_moves_and_mkdirs() {
        let plan = plan_with_ops(vec![
            SyncOp::Move {
                from: PathBuf::from("/dst/old.txt"),
                to: PathBuf::from("/dst/photos/new.txt"),
                is_dir: false,
            },
            SyncOp::MkDir {
                path: PathBuf::from("/dst/photos/sub"),
            },
        ]);
        let out = plan.without_skipped(&["photos".to_owned()]);
        assert!(out.ops.is_empty());
        assert_eq!(out.move_count, 0);
    }

    #[test]
    fn test_summary_zero_counts() {
        let plan = bare_plan(0, 0, 0, 0, 0, 0, 0, 0);
        let s = plan.summary();
        assert!(s.contains("copy=0"), "summary: {s}");
        assert!(s.contains("0.00 B"), "summary: {s}");
    }
}
