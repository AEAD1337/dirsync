use super::fingerprint::hash_file;
use super::walker::FileEntry;
use crate::progress::{LogLevel, ProgressEvent, ProgressState, ScanPhase};
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

const MTIME_TOLERANCE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub enum MatchResult {
    /// File exists at same path, same content: nothing to do.
    Identical,
    /// File exists at same path, same content (verified by hash), but the DST
    /// mtime differs from SRC by more than MTIME_TOLERANCE: touch DST mtime.
    IdenticalMtimeDiverged,
    /// File exists at same path but content differs: overwrite.
    SamePathDifferentContent,
    /// File found in DST at a different path: move it.
    MovedFrom(PathBuf),
    /// File only in SRC: copy to DST.
    NewInSrc,
}

pub struct MatchedEntry {
    pub src: FileEntry,
    pub result: MatchResult,
    /// SHA-256 of the SRC file, if it was computed during matching.
    /// None for files matched by mtime alone or for brand-new files with no
    /// same-size candidates in DST (no hash was needed).
    pub src_hash: Option<[u8; 32]>,
    /// On a case-insensitive DST: the original DST rel-path when this entry
    /// was matched ignoring case (same on-disk file, stored name differs in
    /// case). The planner emits a CaseRename op alongside the content op.
    pub case_renamed_from: Option<PathBuf>,
    /// For a `MovedFrom` match: the moved DST file's mtime is outside the
    /// tolerance, so the planner fixes it in the same run instead of leaving
    /// a TouchMtime for the next one.
    pub touch_after_move: bool,
}

pub struct OrphanEntry {
    pub dst: FileEntry,
}

/// A directory that was renamed: `src_rel` is its current name in SRC,
/// `dst_rel` is its current (old) name in DST.
#[derive(Debug, Clone)]
pub struct RenamedDir {
    pub src_rel: PathBuf,
    pub dst_rel: PathBuf,
}

pub struct MatchOutput {
    pub matched: Vec<MatchedEntry>,
    pub orphans: Vec<OrphanEntry>,
    /// Directories detected as renamed (same files inside, different path).
    pub renamed_dirs: Vec<RenamedDir>,
    /// DST resolves names ignoring case (see `dst_is_case_insensitive`).
    /// The planner then compares paths ignoring case too: otherwise a Copy
    /// of `photo.jpg` and the orphan Delete of `Photo.jpg` hit the same file.
    pub case_insensitive: bool,
}

/// The deepest renamed directory at or above `rel` in `index` (keyed by one
/// side's rel-path), with the rest of `rel` below it. Deepest wins: renames
/// can nest (`a -> y/z` alongside `a/b -> x`), and the inner one is the one
/// that applies to a path below it.
pub(super) fn deepest_rename<'a, 'p>(
    rel: &'p Path,
    index: &HashMap<PathBuf, &'a RenamedDir>,
) -> Option<(&'a RenamedDir, &'p Path)> {
    rel.ancestors()
        .take_while(|a| !a.as_os_str().is_empty())
        .find_map(|a| {
            index
                .get(a)
                .map(|&r| (r, rel.strip_prefix(a).unwrap_or(rel)))
        })
}

/// Two metadata records describe the same file. Unix compares identities;
/// std has no stable file id on Windows, so size and timestamps stand in.
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
            && a.modified().ok() == b.modified().ok()
            && a.created().ok() == b.created().ok()
            && a.file_type() == b.file_type()
    }
}

/// Whether the DST filesystem resolves names ignoring case: NTFS, APFS and
/// HFS+ by default, exFAT/FAT, most SMB shares. Decided by the filesystem,
/// not the OS: macOS and Linux write to such volumes all the time. Probed
/// read-only on an existing DST entry: its name with the ASCII letter case
/// flipped must resolve to the same file. With nothing to probe there is
/// nothing to delete either; fall back to the platform's usual default.
pub fn dst_is_case_insensitive(dst_entries: &[FileEntry]) -> bool {
    for e in dst_entries {
        let Some(name) = e.abs_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let flipped: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        if flipped == name {
            continue; // no ASCII letters to flip
        }
        let other = e.abs_path.with_file_name(flipped);
        match (
            std::fs::symlink_metadata(&e.abs_path),
            std::fs::symlink_metadata(&other),
        ) {
            (Ok(a), Ok(b)) => return same_file(&a, &b),
            (Ok(_), Err(_)) => return false,
            _ => continue,
        }
    }
    cfg!(any(windows, target_os = "macos"))
}

pub fn match_trees(
    src_entries: &[FileEntry],
    dst_entries: &[FileEntry],
    progress: Option<Arc<ProgressState>>,
    drives: crate::drive::DriveProfile,
    cancel: &super::CancelToken,
) -> Result<MatchOutput> {
    let src_files: Vec<&FileEntry> = src_entries
        .iter()
        .filter(|e| !e.is_dir && e.symlink_target.is_none())
        .collect();
    let dst_files: Vec<&FileEntry> = dst_entries
        .iter()
        .filter(|e| !e.is_dir && e.symlink_target.is_none())
        .collect();
    let src_symlinks: Vec<&FileEntry> = src_entries
        .iter()
        .filter(|e| e.symlink_target.is_some())
        .collect();
    let dst_symlinks: Vec<&FileEntry> = dst_entries
        .iter()
        .filter(|e| e.symlink_target.is_some())
        .collect();

    // ------------------------------------------------------------------ //
    // Phase 0: detect renamed directories using content fingerprints.     //
    // Fingerprint = sorted [(path_within_dir, size)] for all files inside.//
    // This is O(n) and requires no I/O: just the size metadata gathered  //
    // during the walk (mtime is not used in the fingerprint).             //
    // ------------------------------------------------------------------ //

    let src_dir_paths: HashSet<&PathBuf> = src_entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| &e.rel_path)
        .collect();
    let dst_dir_paths: HashSet<&PathBuf> = dst_entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| &e.rel_path)
        .collect();

    // Dirs present in SRC but not DST (renamed-to candidates)
    let new_src_dirs: Vec<&FileEntry> = src_entries
        .iter()
        .filter(|e| e.is_dir && !dst_dir_paths.contains(&e.rel_path))
        .collect();

    // Dirs present in DST but not SRC (renamed-from candidates)
    let extra_dst_dirs: Vec<&FileEntry> = dst_entries
        .iter()
        .filter(|e| e.is_dir && !src_dir_paths.contains(&e.rel_path))
        .collect();

    let renamed_dirs = detect_renamed_dirs(&new_src_dirs, &extra_dst_dirs, &src_files, &dst_files);

    // ------------------------------------------------------------------ //
    // Build an "effective DST path" function: files inside a renamed dir  //
    // are looked up as if they already live at the new (SRC) path.        //
    // Indexed by rel-path for O(path_depth) lookups instead of a scan     //
    // over every renamed dir.                                             //
    // ------------------------------------------------------------------ //

    let rename_index: HashMap<PathBuf, &RenamedDir> = renamed_dirs
        .iter()
        .map(|r| (r.dst_rel.clone(), r))
        .collect();

    // Parallel index keyed by src_rel for O(path_depth) inside-renamed-dir checks.
    let src_rename_index: HashMap<PathBuf, &RenamedDir> = renamed_dirs
        .iter()
        .map(|r| (r.src_rel.clone(), r))
        .collect();

    let effective_dst_path = |rel: &PathBuf| -> PathBuf {
        match deepest_rename(rel, &rename_index) {
            Some((rename, suffix)) => rename.src_rel.join(suffix),
            None => rel.clone(),
        }
    };

    // ------------------------------------------------------------------ //
    // Phase 1: classify each SRC file without I/O; collect hash needs.   //
    // ------------------------------------------------------------------ //

    // Pre-compute effective dst paths once; reused for dst_by_path and the
    // orphan filter so effective_dst_path is not called twice per dst file.
    let dst_effective: Vec<PathBuf> = dst_files
        .iter()
        .map(|e| effective_dst_path(&e.rel_path))
        .collect();

    let dst_by_path: HashMap<PathBuf, &FileEntry> = dst_files
        .iter()
        .zip(dst_effective.iter())
        .map(|(e, eff)| (eff.clone(), *e))
        .collect();

    // Case-insensitive DST: secondary index keyed by lowercased effective
    // path. There `a.jpg` and `A.jpg` are the same file: the primary HashMap
    // misses them; this catches the remainder. Empty on a case-sensitive DST.
    let case_insensitive = dst_is_case_insensitive(dst_entries);
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    let dst_by_path_lower: HashMap<String, &FileEntry> = if case_insensitive {
        dst_files
            .iter()
            .zip(dst_effective.iter())
            .map(|(e, eff)| (lower(eff), *e))
            .collect()
    } else {
        HashMap::new()
    };

    // Move-candidate index. DST files with a same-path SRC counterpart are
    // excluded: the Phase-3 pre-pass reserves them before move detection, so
    // they can never be claimed as move sources: hashing them as candidates
    // would be pure waste. The same-path branch below queues its own hashes
    // when content actually needs comparing.
    //
    // Also excluded: DST files the executor must clear *before* the move
    // phase because SRC changes type around them. One sitting where SRC has
    // a directory is removed by that directory's MkDir (Phase 1); one below
    // a path where SRC has a file or symlink is removed with its directory
    // by the hoisted cleanup (Phase 1.5). Claimed as a move source, the
    // first was destroyed before its Move ran (which then renamed the fresh
    // empty directory instead), and the second blocked its own target for
    // ever. As plain orphans they get the Delete + Copy that converges.
    let src_file_paths: HashSet<&PathBuf> = src_files.iter().map(|e| &e.rel_path).collect();
    let src_non_dir_paths: HashSet<&PathBuf> = src_entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| &e.rel_path)
        .collect();
    let cleared_before_moves = |eff: &Path| {
        src_dir_paths.contains(&eff.to_path_buf())
            || eff
                .ancestors()
                .skip(1)
                .any(|a| src_non_dir_paths.contains(&a.to_path_buf()))
    };
    // A case variant of a SRC path is reserved by the pre-pass just like an
    // exact match, so it is no move candidate either.
    let src_file_paths_lower: HashSet<String> = if case_insensitive {
        src_files.iter().map(|e| lower(&e.rel_path)).collect()
    } else {
        HashSet::new()
    };
    let mut dst_by_size: HashMap<u64, Vec<&FileEntry>> = HashMap::new();
    for (e, eff) in dst_files.iter().zip(dst_effective.iter()) {
        if !src_file_paths.contains(eff)
            && !cleared_before_moves(eff)
            && !(case_insensitive && src_file_paths_lower.contains(&lower(eff)))
        {
            dst_by_size.entry(e.size).or_default().push(*e);
        }
    }

    let mut needs_hash = HashQueue::default();

    for src in &src_files {
        if let Some(dst) = dst_by_path.get(&src.rel_path) {
            queue_same_path_hashes(src, dst, dst.rel_path != src.rel_path, &mut needs_hash);
        } else {
            // Case-insensitive fallback: same on-disk file, different stored
            // case. Hash both so Phase 3 can compare content; skip
            // move-detection hashing.
            if case_insensitive && let Some(dst) = dst_by_path_lower.get(&lower(&src.rel_path)) {
                queue_same_path_hashes(src, dst, false, &mut needs_hash);
                continue;
            }
            // No same-path DST file: hash SRC and same-size DST candidates only
            // when candidates exist. Zero-byte files are excluded: their hashes
            // are all identical and move detection is meaningless for them.
            if src.size > 0
                && let Some(candidates) = dst_by_size.get(&src.size)
            {
                needs_hash.src.insert((src.abs_path.clone(), src.size));
                for c in candidates {
                    needs_hash.dst.insert((c.abs_path.clone(), c.size));
                }
            }
        }
    }

    // ------------------------------------------------------------------ //
    // Phase 2: hash needed files, one stream per endpoint.                //
    // Each side runs at its own drive's pace: rayon across all cores for  //
    // an SSD, strictly serial for spinning media. The two streams run     //
    // concurrently because SRC and DST are independent devices: two HDDs //
    // hash simultaneously, each with a single seek stream.                //
    // ------------------------------------------------------------------ //

    // Phase 1 (classification) is CPU-only but O(n): bail before committing
    // to the far more expensive hashing pass.
    cancel.check()?;

    let to_hash_src: Vec<(PathBuf, u64)> = needs_hash.src.into_iter().collect();
    let to_hash_dst: Vec<(PathBuf, u64)> = needs_hash.dst.into_iter().collect();
    let first_to_hash = to_hash_src.first().or(to_hash_dst.first()).cloned();

    // Emit the first file immediately so the CLI/GUI always shows something,
    // then throttle to one event per 500 ms for the rest.
    let hash_start = Instant::now();
    let last_emit_ms = AtomicU64::new(0);
    let throttle_ms: u64 = 100;

    if let Some((first_path, _)) = &first_to_hash
        && let Some(p) = &progress
    {
        let name = first_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        p.emit(ProgressEvent::ScanProgress {
            phase: ScanPhase::Hashing,
            path: name,
        });
        // Seed the timestamp so the rayon loop throttles from this point.
        let now_ms = hash_start.elapsed().as_millis() as u64;
        last_emit_ms.store(now_ms, Ordering::Relaxed);
    }

    let emit_progress = |path: &std::path::Path| {
        if let Some(p) = &progress {
            let now_ms = hash_start.elapsed().as_millis() as u64;
            let prev = last_emit_ms.load(Ordering::Relaxed);
            if now_ms.saturating_sub(prev) >= throttle_ms
                && last_emit_ms
                    .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                p.emit(ProgressEvent::ScanProgress {
                    phase: ScanPhase::Hashing,
                    path: name,
                });
            }
        }
    };

    let log_hash_err = |path: &std::path::Path, err: &anyhow::Error| {
        if let Some(p) = &progress {
            p.emit_log(
                LogLevel::Warning,
                format!("Could not hash {}: {err}", path.display()),
            );
        }
    };

    let hash_one = |path: &PathBuf, size: u64| -> Option<(PathBuf, [u8; 32])> {
        // Once cancelled, every remaining file short-circuits here without
        // touching the disk, so even a 100k-file pass unwinds in milliseconds.
        // The partial map is discarded by the check after the join.
        if cancel.is_cancelled() {
            return None;
        }
        emit_progress(path);
        match hash_file(path, size) {
            Ok(h) => Some((path.clone(), h)),
            Err(e) => {
                log_hash_err(path, &e);
                None
            }
        }
    };

    // One drive's worth of hashing: serial for spinning media (a single seek
    // stream), rayon across all cores otherwise.
    let hash_side = |list: &[(PathBuf, u64)], serial: bool| -> HashMap<PathBuf, [u8; 32]> {
        if serial {
            list.iter()
                .filter_map(|(path, size)| hash_one(path, *size))
                .collect()
        } else {
            list.par_iter()
                .filter_map(|(path, size)| hash_one(path, *size))
                .collect()
        }
    };

    let (src_hashes, dst_hashes) = rayon::join(
        || hash_side(&to_hash_src, drives.src_hdd),
        || hash_side(&to_hash_dst, drives.dst_hdd),
    );
    let mut hashes: HashMap<PathBuf, [u8; 32]> = src_hashes;
    hashes.extend(dst_hashes);

    // A cancelled hash pass leaves `hashes` incomplete; classifying against it
    // would mark unhashed files as content-mismatched or orphaned. Discard it.
    cancel.check()?;

    // ------------------------------------------------------------------ //
    // Phase 2b: build hash → DST-file index for O(1) move detection.   //
    // Only DST files that were hashed (same-size SRC candidates exist)  //
    // are included; SRC hashes are excluded to avoid false matches.     //
    // ------------------------------------------------------------------ //

    let mut dst_by_hash: HashMap<[u8; 32], Vec<&FileEntry>> = HashMap::new();
    for dst in &dst_files {
        if let Some(&hash) = hashes.get(&dst.abs_path) {
            dst_by_hash.entry(hash).or_default().push(dst);
        }
    }

    // ------------------------------------------------------------------ //
    // Phase 3: finalize matches using precomputed hashes.                //
    // ------------------------------------------------------------------ //

    // Track DST files as matched by their effective path.
    let mut dst_matched: HashSet<PathBuf> = HashSet::new();
    let mut matched = Vec::new();

    // Pre-pass: reserve every DST file that has a same-path SRC counterpart.
    // Without this, a SRC file processed earlier in the loop could claim that
    // DST file as a move source before the SRC file at the same path is reached,
    // causing the DST file to be physically moved away while the later same-path
    // SRC file gets an Identical/no-op match: silently deleting it from DST.
    for src in &src_files {
        if dst_by_path.contains_key(&src.rel_path) {
            dst_matched.insert(src.rel_path.clone());
        }
        // Also reserve case-insensitive matches so they are never claimed as
        // move sources by a different SRC file processed earlier.
        if case_insensitive
            && !dst_by_path.contains_key(&src.rel_path)
            && let Some(dst) = dst_by_path_lower.get(&lower(&src.rel_path))
        {
            dst_matched.insert(effective_dst_path(&dst.rel_path));
        }
    }

    for src in &src_files {
        if let Some(dst) = dst_by_path.get(&src.rel_path) {
            // Always resolved in place; mark DST as matched so it is never
            // treated as an orphan or claimed as a move source. Its effective
            // path is the SRC path: that is how dst_by_path found it.
            dst_matched.insert(src.rel_path.clone());
            matched.push(MatchedEntry {
                src: (*src).clone(),
                result: classify_same_path(src, dst, dst.rel_path != src.rel_path, &hashes),
                src_hash: hashes.get(&src.abs_path).copied(),
                case_renamed_from: None,
                touch_after_move: false,
            });
            continue;
        }

        // Case-insensitive path match: same on-disk file, different stored
        // case. Classify by content like a same-path match; record old DST
        // name so the planner can emit a CaseRename alongside any content op.
        if case_insensitive && let Some(dst) = dst_by_path_lower.get(&lower(&src.rel_path)) {
            dst_matched.insert(effective_dst_path(&dst.rel_path));
            matched.push(MatchedEntry {
                src: (*src).clone(),
                result: classify_same_path(src, dst, false, &hashes),
                src_hash: hashes.get(&src.abs_path).copied(),
                case_renamed_from: Some(dst.rel_path.clone()),
                touch_after_move: false,
            });
            continue;
        }

        // No same-path DST counterpart exists: try to find the file at a
        // different DST path (move/rename detection).
        let mut found_move = false;
        // Zero-byte files all share the same hash, so content matching is
        // meaningless for them. Skip move detection to avoid spuriously
        // treating one empty DST file as a "moved" copy of another.
        let is_empty = src.size == 0;
        // Skip move detection for files inside a renamed dir: the dir-level
        // Move op carries them. Defensive: equal fingerprints mean every such
        // file already matched by path above, so this should never fire.
        let inside_renamed = deepest_rename(&src.rel_path, &src_rename_index).is_some();

        if !is_empty && !inside_renamed {
            // Look up by hash directly: O(1) instead of scanning all
            // same-size candidates. `dst_by_hash` only contains DST files,
            // so a hit always means: same content, different path → Move.
            if let Some(&src_hash) = hashes.get(&src.abs_path)
                && let Some(candidates) = dst_by_hash.get(&src_hash)
            {
                for candidate in candidates {
                    let eff = effective_dst_path(&candidate.rel_path);
                    if !dst_matched.contains(&eff) {
                        dst_matched.insert(eff);
                        matched.push(MatchedEntry {
                            src: (*src).clone(),
                            result: MatchResult::MovedFrom(candidate.rel_path.clone()),
                            src_hash: Some(src_hash),
                            case_renamed_from: None,
                            touch_after_move: !mtimes_close(src.mtime, candidate.mtime),
                        });
                        found_move = true;
                        break;
                    }
                }
            }
        }

        if !found_move {
            // No same-path file and no matching DST file found: brand new.
            matched.push(MatchedEntry {
                src: (*src).clone(),
                result: MatchResult::NewInSrc,
                src_hash: hashes.get(&src.abs_path).copied(),
                case_renamed_from: None,
                touch_after_move: false,
            });
        }
    }

    let mut orphans: Vec<OrphanEntry> = dst_files
        .iter()
        .zip(dst_effective.iter())
        .filter(|(_, eff)| !dst_matched.contains(eff.as_path()))
        .map(|(e, _)| OrphanEntry { dst: (*e).clone() })
        .collect();

    // ------------------------------------------------------------------ //
    // Symlink matching: compare targets verbatim, no hashing.            //
    // ------------------------------------------------------------------ //
    // Keyed by *effective* path, like dst_by_path above: a symlink inside a
    // renamed directory is already at its new path by the time the plan runs,
    // so matching on the raw path would classify it as new-in-SRC and leave
    // its DST counterpart looking like an orphan.
    let dst_symlinks_by_path: HashMap<PathBuf, &FileEntry> = dst_symlinks
        .iter()
        .map(|e| (effective_dst_path(&e.rel_path), *e))
        .collect();
    let mut dst_symlinks_matched: HashSet<PathBuf> = HashSet::new();

    for src_sym in &src_symlinks {
        let src_target = src_sym.symlink_target.as_ref().unwrap();
        if let Some(dst_sym) = dst_symlinks_by_path.get(&src_sym.rel_path) {
            dst_symlinks_matched.insert(effective_dst_path(&dst_sym.rel_path));
            let dst_target = dst_sym.symlink_target.as_ref().unwrap();
            let result = if src_target == dst_target {
                MatchResult::Identical
            } else {
                MatchResult::SamePathDifferentContent
            };
            matched.push(MatchedEntry {
                src: (*src_sym).clone(),
                result,
                src_hash: None,
                case_renamed_from: None,
                touch_after_move: false,
            });
        } else {
            // No DST symlink at this path. If a regular file occupies it, the
            // Symlink executor will remove it first; mark as SamePathDifferentContent
            // so the planner knows to emit a Symlink op (not a plain Copy).
            let result = if dst_by_path.contains_key(&src_sym.rel_path) {
                MatchResult::SamePathDifferentContent
            } else {
                MatchResult::NewInSrc
            };
            matched.push(MatchedEntry {
                src: (*src_sym).clone(),
                result,
                src_hash: None,
                case_renamed_from: None,
                touch_after_move: false,
            });
        }
    }

    for dst_sym in &dst_symlinks {
        if !dst_symlinks_matched.contains(&effective_dst_path(&dst_sym.rel_path)) {
            orphans.push(OrphanEntry {
                dst: (*dst_sym).clone(),
            });
        }
    }

    Ok(MatchOutput {
        case_insensitive,
        matched,
        orphans,
        renamed_dirs,
    })
}

/// Build content fingerprint for all files inside `dir_prefix`.
/// Returns [(path_relative_to_dir, size)] in sorted order: cheap, no I/O.
///
/// `sorted_files` **must** be pre-sorted by `rel_path`. The function uses a
/// binary search to skip directly to the relevant range, making each call
/// O(log F + files_in_dir) instead of O(F).
fn dir_fingerprint(sorted_files: &[&FileEntry], dir_prefix: &Path) -> Vec<(PathBuf, u64)> {
    // Jump to the first file whose rel_path is >= dir_prefix.
    // All files inside dir_prefix form a contiguous block starting here.
    let start = sorted_files.partition_point(|f| f.rel_path.as_path() < dir_prefix);
    sorted_files[start..]
        .iter()
        .take_while(|f| f.rel_path.starts_with(dir_prefix))
        .filter_map(|f| {
            f.rel_path
                .strip_prefix(dir_prefix)
                .ok()
                .map(|rel| (rel.to_path_buf(), f.size))
        })
        .collect()
    // No sort needed: input is sorted by rel_path, output preserves that order.
}

fn detect_renamed_dirs(
    new_src_dirs: &[&FileEntry], // SRC dirs with no DST counterpart at same path
    extra_dst_dirs: &[&FileEntry], // DST dirs with no SRC counterpart at same path
    src_files: &[&FileEntry],
    dst_files: &[&FileEntry],
) -> Vec<RenamedDir> {
    if new_src_dirs.is_empty() || extra_dst_dirs.is_empty() {
        return vec![];
    }

    // Sort file lists once so dir_fingerprint can binary-search into them.
    // O(F log F) here replaces O(D_extra × F) across all dir_fingerprint calls.
    let mut sorted_src_files = src_files.to_vec();
    sorted_src_files.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let mut sorted_dst_files = dst_files.to_vec();
    sorted_dst_files.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));

    // Index DST dirs by a 64-bit digest of their fingerprint. Keying on the
    // fingerprint itself stored every file path once per enclosing extra
    // directory (files x depth PathBufs: gigabytes for a reorganised tree of
    // a million files). A digest hit is confirmed against the real
    // fingerprint below, so a collision can only cost a comparison.
    let digest = |fp: &[(PathBuf, u64)]| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        fp.hash(&mut h);
        h.finish()
    };
    let mut dst_by_fp: HashMap<u64, Vec<&FileEntry>> = HashMap::new();
    for dir in extra_dst_dirs {
        let fp = dir_fingerprint(&sorted_dst_files, &dir.rel_path);
        if !fp.is_empty() {
            dst_by_fp.entry(digest(&fp)).or_default().push(dir);
        }
    }
    // True when `path` or one of its ancestors is in `claimed`: O(depth)
    // instead of a scan over every claimed directory.
    let claimed_at_or_above =
        |claimed: &HashSet<PathBuf>, path: &Path| path.ancestors().any(|a| claimed.contains(a));

    // Process SRC dirs shallowest-first so parent renames are claimed before children.
    let mut sorted_src = new_src_dirs.to_vec();
    sorted_src.sort_by_key(|e| e.rel_path.components().count());

    let mut matched_dst: HashSet<PathBuf> = HashSet::new();
    let mut matched_src: HashSet<PathBuf> = HashSet::new();
    let mut result: Vec<RenamedDir> = Vec::new();

    for src_dir in &sorted_src {
        // Skip if this dir is a subdirectory of an already-matched SRC rename.
        if src_dir
            .rel_path
            .parent()
            .is_some_and(|p| claimed_at_or_above(&matched_src, p))
        {
            continue;
        }

        let fp = dir_fingerprint(&sorted_src_files, &src_dir.rel_path);
        if fp.is_empty() {
            continue;
        }

        if let Some(dst_candidates) = dst_by_fp.get(&digest(&fp)) {
            // First DST dir with this fingerprint whose subtree is unclaimed.
            let pick = dst_candidates.iter().find(|d| {
                !claimed_at_or_above(&matched_dst, &d.rel_path)
                    && dir_fingerprint(&sorted_dst_files, &d.rel_path) == fp
            });
            if let Some(dst_dir) = pick {
                result.push(RenamedDir {
                    src_rel: src_dir.rel_path.clone(),
                    dst_rel: dst_dir.rel_path.clone(),
                });
                matched_src.insert(src_dir.rel_path.clone());
                matched_dst.insert(dst_dir.rel_path.clone());
            }
        }
    }

    result
}

fn mtimes_close(a: SystemTime, b: SystemTime) -> bool {
    match a.duration_since(b) {
        Ok(d) => d <= MTIME_TOLERANCE,
        Err(e) => e.duration() <= MTIME_TOLERANCE,
    }
}

/// Classify a SRC file against the DST file at the same (effective) path:
/// size compare, then mtime tolerance, then hash compare. One implementation
/// shared by the exact-case and case-insensitive arms, so a change to the
/// tolerance or the hash guard can never land in one and miss the other.
///
/// `via_rename`: the pair was only brought together by a detected directory
/// rename. That detection trusts sizes alone, so two unrelated directories
/// with the same layout pair up; their files must not take the mtime fast
/// path, or the old content survives under the new name for good.
fn classify_same_path(
    src: &FileEntry,
    dst: &FileEntry,
    via_rename: bool,
    hashes: &HashMap<PathBuf, [u8; 32]>,
) -> MatchResult {
    if src.size != dst.size {
        // Same path, different size: resolved in place as an overwrite;
        // never matched as a move from a different DST path.
        return MatchResult::SamePathDifferentContent;
    }
    let close = mtimes_close(src.mtime, dst.mtime);
    if close && !via_rename {
        return MatchResult::Identical;
    }
    let sh = hashes.get(&src.abs_path);
    let dh = hashes.get(&dst.abs_path);
    match (sh.is_some() && sh == dh, close) {
        (true, true) => MatchResult::Identical,
        (true, false) => MatchResult::IdenticalMtimeDiverged,
        (false, _) => MatchResult::SamePathDifferentContent,
    }
}

/// Queue the hashes a same-path pair needs: both sides when the sizes match
/// but the fast path cannot decide (mtimes diverged, or the pair came from a
/// directory rename), just SRC otherwise (GUI display only). Shared by the
/// exact-case and case-insensitive Phase-1 arms.
///
/// The two sides are queued separately so Phase 2 can drive one hashing
/// stream per drive (see `HashQueue`).
fn queue_same_path_hashes(
    src: &FileEntry,
    dst: &FileEntry,
    via_rename: bool,
    needs_hash: &mut HashQueue,
) {
    if src.size == dst.size {
        if via_rename || !mtimes_close(src.mtime, dst.mtime) {
            needs_hash.src.insert((src.abs_path.clone(), src.size));
            needs_hash.dst.insert((dst.abs_path.clone(), dst.size));
        }
    } else {
        needs_hash.src.insert((src.abs_path.clone(), src.size));
    }
}

/// Files to fingerprint, kept split by endpoint. SRC and DST live on
/// independent devices, so each side is hashed by its own stream at its own
/// drive's pace instead of interleaving both into one queue.
#[derive(Default)]
struct HashQueue {
    src: HashSet<(PathBuf, u64)>,
    dst: HashSet<(PathBuf, u64)>,
}
