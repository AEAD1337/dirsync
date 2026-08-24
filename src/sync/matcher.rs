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
    /// On Windows: the original DST rel-path when this entry was matched
    /// case-insensitively (NTFS path == SRC path, stored name differs in case).
    /// The planner emits a CaseRename op alongside the content op.
    pub case_renamed_from: Option<PathBuf>,
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
    //                                                                      //
    // Index renamed_dirs by dst_rel for O(path_depth) lookups instead of  //
    // the O(renamed_dirs) linear scan the naive closure would produce.     //
    // detect_renamed_dirs guarantees no nesting, so each DST path has at  //
    // most one matching prefix in this map.                                //
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

    // Walk ancestors of `rel` from deepest to shallowest until we find a
    // renamed-dir entry (or exhaust the path, in which case return as-is).
    let effective_dst_path = |rel: &PathBuf| -> PathBuf {
        let mut cur = rel.as_path();
        loop {
            if let Some(rename) = rename_index.get(cur) {
                // cur == rename.dst_rel; suffix is the remaining path below it.
                let suffix = rel.strip_prefix(cur).unwrap();
                return rename.src_rel.join(suffix);
            }
            match cur.parent() {
                Some(p) if !p.as_os_str().is_empty() => cur = p,
                _ => return rel.clone(),
            }
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

    // Windows: secondary index keyed by lowercased effective path for case-insensitive
    // matching. On NTFS (case-insensitive), `a.jpg` and `A.jpg` are the same path:
    // the primary HashMap misses them; this catches the remainder.
    #[cfg(windows)]
    let dst_by_path_lower: HashMap<String, &FileEntry> = dst_files
        .iter()
        .map(|e| {
            let eff = effective_dst_path(&e.rel_path);
            (eff.to_string_lossy().to_lowercase(), *e)
        })
        .collect();

    // Move-candidate index. DST files with a same-path SRC counterpart are
    // excluded: the Phase-3 pre-pass reserves them before move detection, so
    // they can never be claimed as move sources: hashing them as candidates
    // would be pure waste. The same-path branch below queues its own hashes
    // when content actually needs comparing.
    let src_file_paths: HashSet<&PathBuf> = src_files.iter().map(|e| &e.rel_path).collect();
    let mut dst_by_size: HashMap<u64, Vec<&FileEntry>> = HashMap::new();
    for (e, eff) in dst_files.iter().zip(dst_effective.iter()) {
        if !src_file_paths.contains(eff) {
            dst_by_size.entry(e.size).or_default().push(*e);
        }
    }

    let mut needs_hash = HashQueue::default();

    for src in &src_files {
        if let Some(dst) = dst_by_path.get(&src.rel_path) {
            queue_same_path_hashes(src, dst, &mut needs_hash);
        } else {
            // Windows: case-insensitive fallback: same NTFS path, different stored case.
            // Hash both so Phase 3 can compare content; skip move-detection hashing.
            #[cfg(windows)]
            {
                let lower = src.rel_path.to_string_lossy().to_lowercase();
                if let Some(dst) = dst_by_path_lower.get(&lower) {
                    queue_same_path_hashes(src, dst, &mut needs_hash);
                    continue;
                }
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
        // Windows: also reserve case-insensitive matches so they are never
        // claimed as move sources by a different SRC file processed earlier.
        #[cfg(windows)]
        if !dst_by_path.contains_key(&src.rel_path) {
            let lower = src.rel_path.to_string_lossy().to_lowercase();
            if let Some(dst) = dst_by_path_lower.get(&lower) {
                dst_matched.insert(effective_dst_path(&dst.rel_path));
            }
        }
    }

    for src in &src_files {
        if let Some(dst) = dst_by_path.get(&src.rel_path) {
            // Always resolved in place; mark DST as matched so it is never
            // treated as an orphan or claimed as a move source.
            dst_matched.insert(effective_dst_path(&dst.rel_path));
            matched.push(MatchedEntry {
                src: (*src).clone(),
                result: classify_same_path(src, dst, &hashes),
                src_hash: hashes.get(&src.abs_path).copied(),
                case_renamed_from: None,
            });
            continue;
        }

        // Windows: case-insensitive path match: same NTFS path, different stored
        // case. Classify by content like a same-path match; record old DST name so
        // the planner can emit a CaseRename alongside any content op.
        #[cfg(windows)]
        {
            let lower = src.rel_path.to_string_lossy().to_lowercase();
            if let Some(dst) = dst_by_path_lower.get(&lower) {
                dst_matched.insert(effective_dst_path(&dst.rel_path));
                matched.push(MatchedEntry {
                    src: (*src).clone(),
                    result: classify_same_path(src, dst, &hashes),
                    src_hash: hashes.get(&src.abs_path).copied(),
                    case_renamed_from: Some(dst.rel_path.clone()),
                });
                continue;
            }
        }

        // No same-path DST counterpart exists: try to find the file at a
        // different DST path (move/rename detection).
        let mut found_move = false;
        // Zero-byte files all share the same hash, so content matching is
        // meaningless for them. Skip move detection to avoid spuriously
        // treating one empty DST file as a "moved" copy of another.
        let is_empty = src.size == 0;
        // Skip move detection for files inside a renamed dir: the dir-level
        // Move op handles them; the file will be matched by path above.
        // Uses src_rename_index for O(path_depth) instead of O(renamed_dirs).
        let inside_renamed = {
            let mut cur = src.rel_path.as_path();
            let mut found = false;
            loop {
                if src_rename_index.contains_key(cur) {
                    found = true;
                    break;
                }
                match cur.parent() {
                    Some(p) if !p.as_os_str().is_empty() => cur = p,
                    _ => break,
                }
            }
            found
        };

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

    // Index DST dirs by fingerprint for O(1) lookup per SRC dir.
    // fingerprint → list of DST dirs that have that fingerprint.
    let mut dst_by_fp: HashMap<Vec<(PathBuf, u64)>, Vec<&FileEntry>> = HashMap::new();
    for dir in extra_dst_dirs {
        let fp = dir_fingerprint(&sorted_dst_files, &dir.rel_path);
        if !fp.is_empty() {
            dst_by_fp.entry(fp).or_default().push(dir);
        }
    }

    // Process SRC dirs shallowest-first so parent renames are claimed before children.
    let mut sorted_src = new_src_dirs.to_vec();
    sorted_src.sort_by_key(|e| e.rel_path.components().count());

    let mut matched_dst: HashSet<PathBuf> = HashSet::new();
    let mut matched_src: HashSet<PathBuf> = HashSet::new();
    let mut result: Vec<RenamedDir> = Vec::new();

    for src_dir in &sorted_src {
        // Skip if this dir is a subdirectory of an already-matched SRC rename.
        if matched_src
            .iter()
            .any(|p| src_dir.rel_path.starts_with(p) && *p != src_dir.rel_path)
        {
            continue;
        }

        let fp = dir_fingerprint(&sorted_src_files, &src_dir.rel_path);
        if fp.is_empty() {
            continue;
        }

        if let Some(dst_candidates) = dst_by_fp.get(&fp) {
            // Find first unmatched DST dir whose subtree hasn't been claimed yet.
            let pick = dst_candidates.iter().find(|d| {
                !matched_dst.contains(&d.rel_path)
                    && !matched_dst.iter().any(|p| d.rel_path.starts_with(p))
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
/// size compare → mtime tolerance → hash compare. One implementation shared
/// by the exact-case and case-insensitive arms, so a change to the tolerance
/// or the hash guard can never land in one and miss the other.
fn classify_same_path(
    src: &FileEntry,
    dst: &FileEntry,
    hashes: &HashMap<PathBuf, [u8; 32]>,
) -> MatchResult {
    if src.size == dst.size {
        if mtimes_close(src.mtime, dst.mtime) {
            MatchResult::Identical
        } else {
            let sh = hashes.get(&src.abs_path);
            let dh = hashes.get(&dst.abs_path);
            if sh.is_some() && sh == dh {
                MatchResult::IdenticalMtimeDiverged
            } else {
                MatchResult::SamePathDifferentContent
            }
        }
    } else {
        // Same path, different size: resolved in place as an overwrite;
        // never matched as a move from a different DST path.
        MatchResult::SamePathDifferentContent
    }
}

/// Queue the hashes a same-path pair needs: both sides when the sizes match
/// but mtimes diverged (content check), just SRC otherwise (GUI display
/// only). Shared by the exact-case and case-insensitive Phase-1 arms.
///
/// The two sides are queued separately so Phase 2 can drive one hashing
/// stream per drive (see `HashQueue`).
fn queue_same_path_hashes(src: &FileEntry, dst: &FileEntry, needs_hash: &mut HashQueue) {
    if src.size == dst.size {
        if !mtimes_close(src.mtime, dst.mtime) {
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
