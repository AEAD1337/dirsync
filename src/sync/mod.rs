pub mod executor;
pub mod fingerprint;
pub mod matcher;
pub mod planner;
pub mod walker;

#[cfg(test)]
mod tests;

use crate::config::AppConfig;
use crate::error::SkipLog;
use crate::progress::{ProgressEvent, ProgressState, ScanPhase, SyncStatus};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

/// Cooperative cancellation for the preview phases.
///
/// Walking and hashing are long-running blocking work; both poll this between
/// units (per directory entry, per file) so a cancel takes effect promptly
/// instead of only at phase boundaries. `None` means "never cancelled": the
/// engine is usable without a control channel (tests, one-shot previews).
#[derive(Clone, Default)]
pub struct CancelToken(Option<watch::Receiver<bool>>);

impl CancelToken {
    pub fn new(rx: Option<watch::Receiver<bool>>) -> Self {
        Self(rx)
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.as_ref().is_some_and(|rx| *rx.borrow())
    }

    /// `Err("cancelled")` when cancelled: the sentinel `preview()` and the
    /// GUI handler match on to distinguish a cancel from a real failure.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        Ok(())
    }
}

/// Drop every DST entry at or below a SRC path the walk failed to read.
fn shield_unreadable(
    dst_entries: Vec<walker::FileEntry>,
    src_errors: &[walker::WalkError],
) -> Vec<walker::FileEntry> {
    if src_errors.is_empty() {
        return dst_entries;
    }
    dst_entries
        .into_iter()
        .filter(|d| {
            !src_errors
                .iter()
                .any(|e| d.rel_path.starts_with(&e.rel_path))
        })
        .collect()
}

pub struct SyncEngine {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub config: Arc<AppConfig>,
    /// `None` = probe at preview time; `Some` = explicit override.
    pub drives: Option<crate::drive::DriveProfile>,
}

impl SyncEngine {
    pub fn new(src: PathBuf, dst: PathBuf, config: Arc<AppConfig>) -> Self {
        Self {
            src,
            dst,
            config,
            drives: None,
        }
    }

    pub fn with_drives(mut self, drives: crate::drive::DriveProfile) -> Self {
        self.drives = Some(drives);
        self
    }

    /// Force both endpoints to spinning-media behavior.
    pub fn with_hdd(self, hdd: bool) -> Self {
        self.with_drives(crate::drive::DriveProfile::all_hdd(hdd))
    }

    pub async fn preview(
        &self,
        progress: Option<Arc<ProgressState>>,
        cancel_rx: Option<watch::Receiver<bool>>,
    ) -> Result<planner::SyncPlan> {
        let mut out = None;
        self.preview_with(progress, cancel_rx, |plan| out = Some(plan))
            .await?;
        out.ok_or_else(|| anyhow::anyhow!("preview produced no plan"))
    }

    /// `preview`, handing the plan to `deliver` *before* the Previewing
    /// status is released, so a caller that publishes it (the GUI stores it
    /// and emits PlanReady) is done before any observer sees Idle.
    pub async fn preview_with(
        &self,
        progress: Option<Arc<ProgressState>>,
        cancel_rx: Option<watch::Receiver<bool>>,
        deliver: impl FnOnce(planner::SyncPlan),
    ) -> Result<()> {
        let cancel = CancelToken::new(cancel_rx);
        let cancelled = || cancel.is_cancelled();
        // Restore Idle via compare-and-set: only release a status this preview
        // owns. A blind write could clobber a state another actor set (e.g. a
        // run claimed between our completion and the write).
        let release_previewing = |p: &Option<Arc<ProgressState>>| {
            if let Some(p) = p {
                {
                    let mut status = p.status.write().unwrap();
                    if *status != SyncStatus::Previewing {
                        return;
                    }
                    *status = SyncStatus::Idle;
                }
                p.emit(ProgressEvent::StatusChanged {
                    status: SyncStatus::Idle,
                });
            }
        };

        if let Some(p) = &progress {
            *p.status.write().unwrap() = SyncStatus::Previewing;
            p.emit(ProgressEvent::StatusChanged {
                status: SyncStatus::Previewing,
            });
        }

        let excludes = walker::build_excludes(&self.config.exclude_patterns);
        let src = self.src.clone();
        let dst = self.dst.clone();
        let dst_root = self.dst.clone();
        let ex = excludes.clone();

        let prog_src = progress.clone();
        let prog_dst = progress.clone();
        // Probe the drives unless the caller overrode it, so no entrypoint can
        // forget and run 8-way concurrent copies against a spinning disk. The
        // resolved copy policy is stamped on the plan for the executor. (The
        // CLI probes itself so it can print the message before the walk.)
        let drives = match self.drives {
            Some(explicit) => explicit,
            None => {
                // Probing stats both endpoints: off the async workers, which
                // a dead network share would otherwise stall.
                let (src, dst) = (self.src.clone(), self.dst.clone());
                let (profile, drive_msg) =
                    tokio::task::spawn_blocking(move || crate::drive::probe(&src, &dst)).await?;
                if let Some(p) = &progress {
                    p.emit_log(crate::progress::LogLevel::Info, drive_msg);
                    p.emit(ProgressEvent::DriveMode {
                        hdd: profile.serial_copies(),
                    });
                }
                profile
            }
        };
        // Each walk reads exactly one endpoint, and SRC and DST are assumed to
        // be separate devices: so the two walks never contend for the same
        // spindle and always run concurrently, whatever the media type.
        let cancel_src = cancel.clone();
        let cancel_dst = cancel.clone();
        let (src_result, dst_result) = tokio::join!(
            tokio::task::spawn_blocking(move || walker::walk(
                &src,
                &ex,
                "src",
                prog_src,
                &cancel_src
            )),
            tokio::task::spawn_blocking({
                let ex2 = excludes.clone();
                move || {
                    if dst.exists() {
                        walker::walk(&dst, &ex2, "dst", prog_dst, &cancel_dst)
                    } else {
                        Ok(walker::Walk::default())
                    }
                }
            })
        );

        // A cancelled walk returns Err rather than a truncated tree, so this
        // check fires first and turns it into the clean "cancelled" error.
        if cancelled() {
            release_previewing(&progress);
            return Err(anyhow::anyhow!("cancelled"));
        }

        let src_walk = src_result??;
        let dst_walk = dst_result??;
        let src_entries = src_walk.entries;
        // A DST entry at or below a SRC path the walk could not read may well
        // still exist in SRC: leave it out of matching entirely, so it is
        // neither deleted as an orphan nor claimed as a move source.
        let dst_entries = shield_unreadable(dst_walk.entries, &src_walk.errors);
        let walk_errors: Vec<crate::error::FileError> = src_walk
            .errors
            .iter()
            .map(|e| (&self.src, e))
            .chain(dst_walk.errors.iter().map(|e| (&self.dst, e)))
            .map(|(root, e)| crate::error::FileError {
                path: root.join(&e.rel_path),
                message: e.message.clone(),
            })
            .collect();

        if let Some(p) = &progress {
            let src_files = src_entries.iter().filter(|e| !e.is_dir).count();
            let dst_files = dst_entries.iter().filter(|e| !e.is_dir).count();
            p.emit(ProgressEvent::ScanUpdate {
                side: "src".into(),
                file_count: src_files,
            });
            p.emit(ProgressEvent::ScanUpdate {
                side: "dst".into(),
                file_count: dst_files,
            });
        }

        // Check cancel before the expensive parallel-hashing phase.
        if cancelled() {
            release_previewing(&progress);
            return Err(anyhow::anyhow!("cancelled"));
        }

        // Announce fingerprinting phase before the blocking matcher work so
        // the CLI/GUI transitions away from "Walking" even during the CPU-only
        // classification phase (rename-dir detection, size-index building)
        // that runs before any actual I/O.
        if let Some(p) = &progress {
            p.emit(ProgressEvent::ScanProgress {
                phase: ScanPhase::Hashing,
                path: None,
            });
        }

        // Run the blocking matcher (rayon-parallel hashing + classification)
        // on a dedicated blocking thread so tokio worker threads remain free
        // to service the scan() progress display task throughout.
        let prog_for_match = progress.clone();
        let cancel_match = cancel.clone();
        // The entries travel into the blocking task and back, so the
        // planner's directory lists are moved out of them rather than cloned.
        let match_output = tokio::task::spawn_blocking(move || {
            let out = matcher::match_trees(
                &src_entries,
                &dst_entries,
                prog_for_match,
                drives,
                &cancel_match,
            );
            (out, src_entries, dst_entries)
        })
        .await;
        // Same contract as the walks: a cancelled match returns Err instead of
        // a partial classification (which would read as mass orphans).
        if cancelled() {
            release_previewing(&progress);
            return Err(anyhow::anyhow!("cancelled"));
        }
        let (match_output, src_entries, dst_entries) = match_output?;
        let match_output = match_output?;
        let src_dirs: Vec<_> = src_entries.into_iter().filter(|e| e.is_dir).collect();
        let dst_dirs: Vec<_> = dst_entries.into_iter().filter(|e| e.is_dir).collect();

        if let Some(p) = &progress {
            p.emit(ProgressEvent::ScanProgress {
                phase: ScanPhase::Planning,
                path: None,
            });
        }

        let src_root = self.src.clone();
        let mut plan = planner::plan(
            match_output,
            &src_dirs,
            &dst_dirs,
            &src_root,
            &dst_root,
            drives.serial_copies(),
        );
        plan.walk_errors = walk_errors;

        deliver(plan);
        release_previewing(&progress);

        Ok(())
    }

    pub async fn run(
        &self,
        plan: planner::SyncPlan,
        progress: Arc<ProgressState>,
        dry_run: bool,
        pause_rx: watch::Receiver<bool>,
        cancel_rx: watch::Receiver<bool>,
    ) -> SkipLog {
        // Ensure DST root exists
        if !dry_run {
            let dst = self.dst.clone();
            let _ = tokio::task::spawn_blocking(move || std::fs::create_dir_all(dst)).await;
        }
        // The plan carries the drive mode its preview resolved: using it here
        // keeps run consistent with preview regardless of engine construction.
        let hdd = plan.hdd;
        executor::execute(
            plan,
            progress,
            executor::ExecuteOptions { dry_run, hdd },
            pause_rx,
            cancel_rx,
        )
        .await
    }
}
