use crate::progress::{LogLevel, ProgressEvent, ProgressState, ScanPhase};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;
use wildmatch::WildMatch;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub rel_path: PathBuf,
    pub abs_path: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
    pub is_dir: bool,
    /// Non-None when this entry is a symlink; holds the raw link target (not followed).
    pub symlink_target: Option<PathBuf>,
}

#[derive(Clone)]
pub struct ExcludeSet(Vec<WildMatch>);

impl ExcludeSet {
    pub fn is_match(&self, s: &std::ffi::OsStr) -> bool {
        let s = s.to_string_lossy();
        self.0.iter().any(|p| p.matches(&s))
    }
}

pub fn walk(
    root: &Path,
    excludes: &ExcludeSet,
    side: &str,
    progress: Option<Arc<ProgressState>>,
    cancel: &super::CancelToken,
) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    let throttle = Duration::from_millis(500);
    let mut last_emit = Instant::now() - throttle;

    // Excluded names are pruned at the point of descent: filter_entry stops
    // WalkDir from even readdir-ing an excluded subtree, instead of
    // enumerating everything below it and rejecting each entry afterwards.
    // Per-component semantics are preserved: a path whose ancestor matches
    // never appears because that ancestor itself was pruned.
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !excludes.is_match(e.file_name()));
    for entry in walker {
        // Polled per entry so a cancel lands mid-walk instead of only at the
        // phase boundary: a large or slow (network) tree can take minutes.
        // A `watch` borrow is an uncontended read lock, negligible next to the
        // readdir/metadata syscall each entry already costs.
        //
        // Bailing with Err, never the entries collected so far: a truncated
        // tree is worse than no tree. A partial DST walk makes real files look
        // like orphans, and orphans get deleted.
        cancel.check()?;

        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                if let Some(p) = &progress {
                    p.emit_log(LogLevel::Warning, format!("Walk error: {e}"));
                } else {
                    eprintln!("Warning: walk error: {e}");
                }
                continue;
            }
        };

        if entry.depth() == 0 {
            continue;
        }

        // Log and skip rather than `?`: every other per-entry failure in this
        // loop continues, and one odd entry should not abort the whole walk.
        let rel_path = match entry.path().strip_prefix(root) {
            Ok(p) => p.to_path_buf(),
            Err(e) => {
                if let Some(p) = &progress {
                    p.emit_log(
                        LogLevel::Warning,
                        format!("Skipping {}: {e}", entry.path().display()),
                    );
                } else {
                    eprintln!("Warning: skipping {}: {e}", entry.path().display());
                }
                continue;
            }
        };

        if let Some(p) = &progress {
            let now = Instant::now();
            if now.duration_since(last_emit) >= throttle {
                last_emit = now;
                let path_str = Some(crate::paths::to_slash(&rel_path));
                p.emit(ProgressEvent::ScanProgress {
                    phase: ScanPhase::Walking {
                        side: side.to_owned(),
                    },
                    path: path_str,
                });
            }
        }

        // Preserve symlinks as-is; never follow them into their target.
        if entry.path_is_symlink() {
            match std::fs::read_link(entry.path()) {
                Ok(target) => entries.push(FileEntry {
                    rel_path,
                    abs_path: entry.path().to_path_buf(),
                    size: 0,
                    mtime: SystemTime::UNIX_EPOCH,
                    is_dir: false,
                    symlink_target: Some(target),
                }),
                Err(e) => {
                    if let Some(p) = &progress {
                        p.emit_log(
                            LogLevel::Warning,
                            format!("Cannot read symlink {}: {e}", entry.path().display()),
                        );
                    } else {
                        eprintln!(
                            "Warning: cannot read symlink {}: {e}",
                            entry.path().display()
                        );
                    }
                }
            }
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                if let Some(p) = &progress {
                    p.emit_log(
                        LogLevel::Warning,
                        format!("Cannot read metadata for {}: {e}", entry.path().display()),
                    );
                } else {
                    eprintln!(
                        "Warning: cannot read metadata for {}: {e}",
                        entry.path().display()
                    );
                }
                continue;
            }
        };

        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let size = if metadata.is_dir() { 0 } else { metadata.len() };

        entries.push(FileEntry {
            rel_path,
            abs_path: entry.path().to_path_buf(),
            size,
            mtime,
            is_dir: metadata.is_dir(),
            symlink_target: None,
        });
    }

    if let Some(p) = &progress {
        p.emit(ProgressEvent::ScanProgress {
            phase: ScanPhase::Walking {
                side: side.to_owned(),
            },
            path: Some("Done.".to_string()),
        });
    }

    Ok(entries)
}

/// Patterns that are always excluded regardless of user configuration.
///
/// The first group is Windows-specific directories that appear at drive roots
/// and must never be touched by a sync operation.
///
/// The rest are our own staging files. A copy killed mid-write leaves one
/// behind; if it were walked it would be hashed and could be claimed as a move
/// source in the next run, making leftover litter load-bearing. Excluding them
/// keeps matching honest: they are simply ignored until overwritten or removed
/// by hand.
const BUILTIN_EXCLUDES: &[&str] = &[
    "System Volume Information",
    "WindowsApps",
    "$Recycle.Bin",
    "$RECYCLE.BIN",
    "*.__dirsync_tmp__",
    "*.__dirsync_case__",
    ".*.__dirsync_swap_*__",
];

pub fn build_excludes(patterns: &[String]) -> Result<ExcludeSet> {
    let matchers = BUILTIN_EXCLUDES
        .iter()
        .map(|p| WildMatch::new(p))
        .chain(patterns.iter().map(|p| WildMatch::new(p)))
        .collect();
    Ok(ExcludeSet(matchers))
}
