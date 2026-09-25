use super::planner::{LinkKind, OP_TOKEN_BYTES, SyncOp, SyncPlan};
use crate::error::SkipLog;
use crate::progress::{ProgressEvent, ProgressState, SyncStatus};
use anyhow::Result;
use filetime::{FileTime, set_file_mtime};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

const COPY_BUF: usize = 256 * 1024;

/// Suffix for the same-directory staging file both copy paths rename from.
pub(crate) const TMP_SUFFIX: &str = ".__dirsync_tmp__";

/// Stage path for a copy into `dst`: same directory, so the rename is always
/// same-filesystem and therefore atomic.
fn tmp_path_for(dst: &Path) -> PathBuf {
    dst.with_file_name(format!(
        "{}{TMP_SUFFIX}",
        dst.file_name().unwrap_or_default().to_string_lossy()
    ))
}

/// Remove a symlink occupying a copy target so the staging rename can land.
///
/// The planner suppresses the orphan Delete for any path a write op targets
/// (`occupied_dsts`), so a DST symlink sitting where SRC has a regular file is
/// never cleaned up by a Delete op: it has to be cleared here. Removing a
/// symlink does not touch whatever it points at.
///
/// Real directories are *not* handled here: `remove_dir_all` would destroy
/// excluded files that the plan deliberately left alone. Those are cleared by
/// running their Delete/RmDir ops before the copy phase instead (see
/// `blocking_copy_*` in `execute`), which preserves RmDir's refusal to remove a
/// non-empty directory.
fn clear_symlink_at(dst: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(dst).or_else(|_| std::fs::remove_dir(dst))
        }
        _ => Ok(()),
    }
}

/// Clear the Windows readonly attribute on an existing regular file. A
/// readonly DST file (mirrored from a readonly SRC file) otherwise fails
/// every later rename onto it and every mtime write with "Access is denied".
/// Unix readonly modes block neither for the owner, so this is Windows-only.
#[cfg(windows)]
fn clear_readonly(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && meta.permissions().readonly() => {
            let mut perms = meta.permissions();
            // Clearing the attribute is exactly the intent on Windows.
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(path, perms)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(not(windows))]
fn clear_readonly(_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

/// Run `f` against `path` with its readonly attribute lifted, restoring it
/// afterwards (TouchMtime on a readonly mirror).
fn with_writable<T>(path: &Path, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let was_readonly = clear_readonly(path)?;
    let result = f();
    if was_readonly {
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms)?;
    }
    result
}

/// Stage a copy of `src` via `write` (which fills the staging path it is
/// given), then commit it over `dst` with one atomic rename. Shared by both
/// copy paths so they cannot drift apart on safety or metadata:
///
/// - A source whose size or mtime changed while it was being copied (a
///   database or VM image rewritten in place) fails the op. Committing it
///   would leave a torn copy that every later run calls Identical, because
///   the fast path compares exactly those two fields.
/// - The copy carries the mtime and permissions the source had *before* the
///   copy, whichever path wrote it (`fs::copy` mirrors attributes on its own,
///   the chunked loop does not).
/// - A readonly DST file is replaced rather than failing forever.
/// - Any failure removes the staging file: left behind it litters DST.
pub(crate) fn stage_and_commit(
    src: &Path,
    dst: &Path,
    write: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let before = fs::metadata(src)?;
    let tmp = tmp_path_for(dst);
    let staged = (|| -> Result<()> {
        write(&tmp)?;
        let after = fs::metadata(src)?;
        if after.len() != before.len() || after.modified().ok() != before.modified().ok() {
            anyhow::bail!("source changed during copy; not committed, the next run retries it");
        }
        clear_readonly(&tmp)?;
        if let Ok(mtime) = before.modified() {
            set_file_mtime(&tmp, FileTime::from_system_time(mtime))?;
        }
        fs::set_permissions(&tmp, before.permissions())?;
        clear_symlink_at(dst)?;
        clear_readonly(dst)?;
        fs::rename(&tmp, dst)?;
        Ok(())
    })();
    if staged.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    staged
}

/// Files at or below this size use fs::copy (single syscall, no chunk loop)
/// and are executed in parallel. Above it, the chunked progress-aware path runs.
const SMALL_FILE: u64 = 1024 * 1024; // 1 MB

/// Max concurrent small-file copy workers.
const COPY_JOBS: usize = 8;

#[derive(Clone, Copy)]
pub struct ExecuteOptions {
    pub dry_run: bool,
    /// HDD-friendly mode: run all copies serially, one at a time.
    pub hdd: bool,
}

/// The path written / created / deleted by `op`: every op variant writes
/// exactly one destination path. Doubles as the op's display path for
/// progress reporting.
fn write_target(op: &SyncOp) -> &Path {
    match op {
        SyncOp::Copy { dst, .. } => dst,
        SyncOp::Overwrite { dst, .. } => dst,
        SyncOp::Move { to, .. } => to,
        SyncOp::Delete { path, .. } => path,
        SyncOp::MkDir { path } => path,
        SyncOp::RmDir { path } => path,
        SyncOp::TouchMtime { dst, .. } => dst,
        SyncOp::Symlink { dst, .. } => dst,
        SyncOp::CaseRename { to, .. } => to.as_path(),
    }
}

pub async fn execute(
    plan: SyncPlan,
    progress: Arc<ProgressState>,
    opts: ExecuteOptions,
    pause_rx: watch::Receiver<bool>,
    cancel_rx: watch::Receiver<bool>,
) -> SkipLog {
    let mut skip_log = SkipLog::default();

    // Safety gate: every path an op writes or removes must be inside
    // dst_root. The planner builds all of them from dst_root, but we verify
    // here so that a future planner bug or a crafted CLI invocation can never
    // touch SRC or any other unrelated directory. A rename's source counts:
    // the file disappears from there. `..` is refused outright because
    // `starts_with` is purely textual.
    for op in &plan.ops {
        let escapes = |p: &Path| {
            !p.starts_with(&plan.dst_root)
                || p.components().any(|c| c == std::path::Component::ParentDir)
        };
        let source = match op {
            SyncOp::Move { from, .. } | SyncOp::CaseRename { from, .. } => Some(from.as_path()),
            _ => None,
        };
        let target = match source {
            Some(from) if escapes(from) => from,
            _ => write_target(op),
        };
        if escapes(target) {
            let msg = format!(
                "SAFETY: refusing to execute op whose target '{}' is outside dst_root '{}': aborting run",
                target.display(),
                plan.dst_root.display(),
            );
            eprintln!("{msg}");
            skip_log.push(target.to_path_buf(), msg);
            *progress.status.write().unwrap() = SyncStatus::Cancelled;
            progress.emit(ProgressEvent::StatusChanged {
                status: SyncStatus::Cancelled,
            });
            return skip_log;
        }
    }

    let planned_ops = plan.ops.len();
    let planned_bytes = plan.total_bytes;
    *progress.status.write().unwrap() = SyncStatus::Running;
    progress.emit(ProgressEvent::StatusChanged {
        status: SyncStatus::Running,
    });

    // Partition into ordered execution phases
    let mut mkdirs: Vec<SyncOp> = vec![];
    let mut moves: Vec<SyncOp> = vec![];
    let mut case_renames: Vec<SyncOp> = vec![];
    let mut symlinks: Vec<SyncOp> = vec![];
    let mut copies: Vec<SyncOp> = vec![];
    let mut touches: Vec<SyncOp> = vec![];
    let mut deletes: Vec<SyncOp> = vec![];
    let mut rmdirs: Vec<SyncOp> = vec![];
    for op in plan.ops {
        match op {
            SyncOp::MkDir { .. } => mkdirs.push(op),
            SyncOp::Move { .. } => moves.push(op),
            SyncOp::CaseRename { .. } => case_renames.push(op),
            SyncOp::Symlink { .. } => symlinks.push(op),
            SyncOp::Copy { .. } | SyncOp::Overwrite { .. } => copies.push(op),
            SyncOp::TouchMtime { .. } => touches.push(op),
            SyncOp::Delete { .. } => deletes.push(op),
            SyncOp::RmDir { .. } => rmdirs.push(op),
        }
    }

    // A write target that DST currently holds as a *directory* must be cleared
    // before the write executes: neither the copy's staging rename, `fs::rename`
    // for a file-level move, nor symlink creation can replace a directory, and
    // that directory's own Delete/RmDir ops do not run until phases 4 and 5.
    // The planner flagged these targets from its DST walk (zero I/O: the old
    // per-target stat pass here ran serially on the async runtime); hoist
    // exactly those targets' ops forward (Phase 1.5, before any write phase).
    // It keeps RmDir's refusal to remove a non-empty directory, so excluded
    // content still blocks the write instead of being destroyed.
    let write_targets: std::collections::HashSet<PathBuf> =
        plan.dir_blocked_targets.into_iter().collect();

    let (blocking_write_deletes, deletes): (Vec<_>, Vec<_>) = if write_targets.is_empty() {
        (vec![], deletes)
    } else {
        deletes.into_iter().partition(|op| match op {
            SyncOp::Delete { path, .. } => path.ancestors().any(|a| write_targets.contains(a)),
            _ => false,
        })
    };
    let (blocking_write_rmdirs, rmdirs): (Vec<_>, Vec<_>) = if write_targets.is_empty() {
        (vec![], rmdirs)
    } else {
        rmdirs.into_iter().partition(|op| match op {
            SyncOp::RmDir { path } => path.ancestors().any(|a| write_targets.contains(a)),
            _ => false,
        })
    };

    // Sort file moves up front: the cycle breaker can append temp-rename ops
    // beyond the planned list, and the progress denominator has to include
    // them or ops_done finishes above ops_total. Every other phase is a
    // partition of plan.ops, so this is the only source of extra ops.
    let (dir_moves, file_moves): (Vec<_>, Vec<_>) = moves
        .into_iter()
        .partition(|op| matches!(op, SyncOp::Move { is_dir: true, .. }));
    let planned_file_moves = file_moves.len();
    let sorted_file_moves = sort_file_moves(file_moves);
    let extra_ops = sorted_file_moves.len().saturating_sub(planned_file_moves);

    let dir_move_targets: std::collections::HashSet<PathBuf> = dir_moves
        .iter()
        .filter_map(|op| {
            if let SyncOp::Move { to, .. } = op {
                Some(to.clone())
            } else {
                None
            }
        })
        .collect();

    // MkDirs for brand-new dirs inside a renamed subtree target the
    // post-rename path; running them in Phase 1 would materialize the rename
    // target and make the dir move fail, so they run right after the moves.
    let (post_move_mkdirs, mkdirs): (Vec<_>, Vec<_>) = mkdirs.into_iter().partition(|op| {
        if let SyncOp::MkDir { path } = op {
            path.ancestors().any(|a| dir_move_targets.contains(a))
        } else {
            false
        }
    });

    progress.reset(
        planned_bytes + extra_ops as u64 * OP_TOKEN_BYTES,
        planned_ops + extra_ops,
    );
    let ctl = Control {
        progress: &progress,
        opts,
        pause_rx: &pause_rx,
        cancel_rx: &cancel_rx,
    };

    // Phase 1: MkDir: serial (must precede all writes, very fast)
    if !run_serial(mkdirs, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    // Phase 1.5: clear DST directories that sit where a write op will land, by
    // running their own Delete/RmDir ops early (deepest-first order preserved
    // from the planner). A non-empty directory left by excluded content still
    // survives RmDir, and the write then fails with a clear error rather than
    // silently destroying data the user excluded.
    if !run_serial(
        blocking_write_deletes
            .into_iter()
            .chain(blocking_write_rmdirs),
        &ctl,
        &mut skip_log,
    )
    .await
    {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }
    // A directory that survived its RmDir still holds content the plan never
    // touches (excluded names): report what blocks it, and drop the writes
    // that would only fail against it with a bare "Access is denied".
    let still_blocked: std::collections::HashSet<PathBuf> = if opts.dry_run {
        Default::default()
    } else {
        write_targets
            .iter()
            .filter(|t| fs::symlink_metadata(t).is_ok_and(|m| m.is_dir()))
            .cloned()
            .collect()
    };
    for dir in &still_blocked {
        let names: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .take(3)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let msg = format!(
            "cannot replace this directory: it still holds content excluded from sync ({})",
            names.join(", ")
        );
        skip_log.push(dir.clone(), msg.clone());
        progress.emit(ProgressEvent::FileError {
            name: dir.display().to_string(),
            message: msg,
        });
    }
    let not_blocked = |op: &SyncOp| !still_blocked.contains(write_target(op));
    copies.retain(not_blocked);
    symlinks.retain(not_blocked);
    let sorted_file_moves: Vec<SyncOp> =
        sorted_file_moves.into_iter().filter(not_blocked).collect();
    let dir_moves: Vec<SyncOp> = dir_moves.into_iter().filter(not_blocked).collect();

    // Phase 2: Moves: serial (rename is near-instant, no benefit from parallelism)
    // File-level moves run first, topologically sorted to handle chains and
    // cycles; dir-level renames follow. The two sets are independent - a
    // detected dir rename requires an identical recursive file fingerprint, so
    // no file move can read from or write into a renamed subtree - and running
    // file moves first lets a move vacate a file that occupies a dir-move
    // target (that file is a claimed move source, so it has no Delete op the
    // pre-deletion below could hoist).
    //
    // Before any move: delete any DST files that occupy a dir-move target path.
    // SRC and DST are independent trees, so a DST file can legally share a name
    // with a new SRC directory. Without this pre-deletion the rename syscall
    // would fail (ENOTDIR on POSIX, ERROR_ALREADY_EXISTS on Windows).
    let (blocking_deletes, deletes): (Vec<_>, Vec<_>) = deletes.into_iter().partition(|op| {
        if let SyncOp::Delete { path, .. } = op {
            dir_move_targets.contains(path)
        } else {
            false
        }
    });

    if !run_serial(blocking_deletes, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    let moves: Vec<_> = sorted_file_moves.into_iter().chain(dir_moves).collect();

    if !run_serial(moves, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    // Phase 2.1: MkDirs inside renamed subtrees: deferred until after the
    // dir moves that create their parents (see partition above).
    if !run_serial(post_move_mkdirs, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    // Phase 2.5: CaseRenames: serial; two-step rename to force the case
    // update on a case-insensitive filesystem. Dir case renames run first so
    // their children resolve correctly in Phase 3.
    {
        let (dir_case_renames, file_case_renames): (Vec<_>, Vec<_>) = case_renames
            .into_iter()
            .partition(|op| matches!(op, SyncOp::CaseRename { is_dir: true, .. }));
        if !run_serial(
            dir_case_renames.into_iter().chain(file_case_renames),
            &ctl,
            &mut skip_log,
        )
        .await
        {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
    }

    // Phase 3: Symlinks: serial, near-instant (no data to copy).
    if !run_serial(symlinks, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    // Phase 3a: small copies.
    // Default: concurrent up to COPY_JOBS workers (fast path via fs::copy).
    // HDD mode: serial: no concurrent seeks on the same physical spindle.
    let (small_copies, large_copies): (Vec<_>, Vec<_>) = copies.into_iter().partition(|op| {
        matches!(op, SyncOp::Copy { size, .. } | SyncOp::Overwrite { size, .. } if *size <= SMALL_FILE)
    });

    if opts.hdd {
        // Serial path: reuse run_one so the progress accounting is identical.
        if !run_serial(small_copies, &ctl, &mut skip_log).await {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
    } else if !small_copies.is_empty() {
        let sem = Arc::new(Semaphore::new(COPY_JOBS));
        let mut tasks: JoinSet<(PathBuf, Result<String>)> = JoinSet::new();

        for op in small_copies {
            if *cancel_rx.borrow() {
                break;
            }
            wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
            if *cancel_rx.borrow() {
                break;
            }
            let permit = sem.clone().acquire_owned().await.unwrap();
            let progress = progress.clone();
            let dry_run = opts.dry_run;
            tasks.spawn(async move {
                let _permit = permit;
                let path = write_target(&op).to_path_buf();
                let result = do_copy_small(op, &progress, dry_run).await;
                (path, result)
            });
        }

        // Drain: we always wait for all in-flight copies even if cancelled,
        // since spawn_blocking cannot be safely aborted mid-write.
        while let Some(join_result) = tasks.join_next().await {
            let (path, result) = join_result.expect("copy task panicked");
            match result {
                Ok(summary) => {
                    progress.ops_done.fetch_add(1, Ordering::Relaxed);
                    progress.emit(ProgressEvent::OpDone {
                        summary,
                        path: path.to_string_lossy().into_owned(),
                    });
                }
                Err(e) => {
                    skip_log.push(path.clone(), e.to_string());
                    progress.emit(ProgressEvent::FileError {
                        name: path.display().to_string(),
                        message: e.to_string(),
                    });
                }
            }
        }

        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
    }

    // Phase 3b: large copies: sequential, with per-chunk progress reporting.
    if !run_serial(large_copies, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    // Phase 3c: TouchMtime: serial, cheap metadata-only writes.
    if !run_serial(touches, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    // Phase 4: Deletes: serial
    if !run_serial(deletes, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    // Phase 5: RmDir: serial (deepest-first order from planner)
    if !run_serial(rmdirs, &ctl, &mut skip_log).await {
        return set_status(skip_log, SyncStatus::Cancelled, &progress);
    }

    // A cancel that interrupted the final op (a long copy polls it mid-file)
    // passed every between-op check: the run did not finish, so it must not
    // read as Done.
    let status = if *cancel_rx.borrow() {
        SyncStatus::Cancelled
    } else {
        SyncStatus::Done
    };
    set_status(skip_log, status, &progress)
}

/// What every serial phase needs to run its ops.
struct Control<'a> {
    progress: &'a Arc<ProgressState>,
    opts: ExecuteOptions,
    pause_rx: &'a watch::Receiver<bool>,
    cancel_rx: &'a watch::Receiver<bool>,
}

/// Run `ops` one at a time, honouring pause and cancel between them.
/// Returns `false` when a cancel stopped the phase.
async fn run_serial(
    ops: impl IntoIterator<Item = SyncOp>,
    ctl: &Control<'_>,
    skip_log: &mut SkipLog,
) -> bool {
    for op in ops {
        if *ctl.cancel_rx.borrow() {
            return false;
        }
        wait_if_paused(ctl.pause_rx, ctl.cancel_rx, ctl.progress).await;
        // A cancel that arrived while paused must stop the op the run was
        // paused in front of, not just the ones after it.
        if *ctl.cancel_rx.borrow() {
            return false;
        }
        run_one(op, ctl.progress, ctl.opts, skip_log, ctl.cancel_rx).await;
    }
    true
}

/// Topologically sort file-level move operations so that they execute in a
/// safe order.  The core constraint: if Move A writes to path P and Move B
/// reads from path P, B must execute before A (otherwise A clobbers B's
/// source).
///
/// When a cycle exists (e.g. a↔b swap), we break it by inserting a temporary
/// rename that saves one file's content before it would be overwritten.
fn sort_file_moves(file_moves: Vec<SyncOp>) -> Vec<SyncOp> {
    let n = file_moves.len();
    if n <= 1 {
        return file_moves;
    }

    // Decompose each Move into (from, to, is_dir). We may patch `from` when
    // breaking cycles.
    let mut entries: Vec<(PathBuf, PathBuf, bool)> = file_moves
        .into_iter()
        .map(|op| {
            if let SyncOp::Move { from, to, is_dir } = op {
                (from, to, is_dir)
            } else {
                unreachable!()
            }
        })
        .collect();

    // A move is ready when no other pending move will read from the path it
    // writes to. Every `from` is a distinct DST path (each claimed once by
    // the matcher) and every `to` a distinct SRC path, so each move has at
    // most one blocker and blocks at most one other: the dependency graph
    // is plain chains and simple cycles, solvable in O(n) with a Kahn-style
    // scan instead of an O(n²) ready-search per emitted op.
    let from_index: std::collections::HashMap<PathBuf, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, (from, _, _))| (from.clone(), i))
        .collect();

    // Edge j → i when move j reads the path move i writes: j must run first.
    // succ[j] is that i; in_deg[i] counts (0 or 1) unfinished predecessors.
    let mut in_deg = vec![0u8; n];
    let mut succ: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        if let Some(&j) = from_index.get(&entries[i].1)
            && j != i
        {
            in_deg[i] = 1;
            succ[j] = Some(i);
        }
    }

    let mut pending = vec![true; n];
    let mut result: Vec<SyncOp> = Vec::with_capacity(n + 4); // +4 for potential cycle-breakers
    let mut queue: std::collections::VecDeque<usize> = (0..n).filter(|&i| in_deg[i] == 0).collect();
    let mut emitted = 0usize;
    // Monotone cursor for cycle detection: indices it passes are never
    // pending again, so the total scan cost stays O(n) across all cycles.
    let mut cursor = 0usize;

    let unblock = |k: usize,
                   in_deg: &mut Vec<u8>,
                   pending: &[bool],
                   queue: &mut std::collections::VecDeque<usize>| {
        if pending[k] && in_deg[k] > 0 {
            in_deg[k] -= 1;
            if in_deg[k] == 0 {
                queue.push_back(k);
            }
        }
    };

    while emitted < n {
        while let Some(i) = queue.pop_front() {
            let (from, to, is_dir) = entries[i].clone();
            result.push(SyncOp::Move { from, to, is_dir });
            pending[i] = false;
            emitted += 1;
            if let Some(k) = succ[i] {
                unblock(k, &mut in_deg, &pending, &mut queue);
            }
        }
        if emitted == n {
            break;
        }
        // Everything still pending sits on a cycle. Break one by saving the
        // first pending move's source to a temp path; the move that was
        // blocked on that path can then proceed and the chain unwinds, with
        // the patched move reading the temp file at its turn.
        while !pending[cursor] {
            cursor += 1;
        }
        let i = cursor;
        let fname = entries[i]
            .0
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let tmp = entries[i]
            .0
            .with_file_name(format!(".{}.__dirsync_swap_{}__", fname, i));
        result.push(SyncOp::Move {
            from: entries[i].0.clone(),
            to: tmp.clone(),
            is_dir: entries[i].2,
        });
        entries[i].0 = tmp;
        // i no longer reads its original source, so the move that writes that
        // path is unblocked.
        if let Some(k) = succ[i] {
            unblock(k, &mut in_deg, &pending, &mut queue);
            succ[i] = None;
        }
    }

    result
}

fn set_status(log: SkipLog, status: SyncStatus, progress: &Arc<ProgressState>) -> SkipLog {
    if matches!(status, SyncStatus::Done | SyncStatus::Cancelled) {
        progress.stop_timer();
    }
    *progress.status.write().unwrap() = status.clone();
    progress.emit(ProgressEvent::StatusChanged { status });
    log
}

async fn wait_if_paused(
    pause_rx: &watch::Receiver<bool>,
    cancel_rx: &watch::Receiver<bool>,
    progress: &Arc<ProgressState>,
) {
    if !*pause_rx.borrow() {
        return;
    }
    progress.pause_timer();
    *progress.status.write().unwrap() = SyncStatus::Paused;
    progress.emit(ProgressEvent::StatusChanged {
        status: SyncStatus::Paused,
    });
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if *cancel_rx.borrow() || !*pause_rx.borrow() {
            break;
        }
    }
    if !*cancel_rx.borrow() {
        progress.resume_timer();
        *progress.status.write().unwrap() = SyncStatus::Running;
        progress.emit(ProgressEvent::StatusChanged {
            status: SyncStatus::Running,
        });
    }
}

async fn run_one(
    op: SyncOp,
    progress: &Arc<ProgressState>,
    opts: ExecuteOptions,
    skip_log: &mut SkipLog,
    cancel_rx: &watch::Receiver<bool>,
) {
    let is_token_op = !matches!(op, SyncOp::Copy { .. } | SyncOp::Overwrite { .. });
    let path = write_target(&op).to_path_buf();
    match execute_op(op, progress, opts.dry_run, cancel_rx).await {
        Ok(summary) => {
            if is_token_op {
                progress.record_bytes(OP_TOKEN_BYTES);
            }
            progress.ops_done.fetch_add(1, Ordering::Relaxed);
            progress.emit(ProgressEvent::OpDone {
                summary,
                path: path.to_string_lossy().into_owned(),
            });
        }
        // A cancel that interrupts an op (the chunked copy's sentinel) is the
        // user's decision, not a failure of that file.
        Err(e) if *cancel_rx.borrow() && e.to_string() == "cancelled" => {}
        Err(e) => {
            skip_log.push(path.clone(), e.to_string());
            progress.emit(ProgressEvent::FileError {
                name: path.display().to_string(),
                message: e.to_string(),
            });
        }
    }
}

async fn execute_op(
    op: SyncOp,
    progress: &Arc<ProgressState>,
    dry_run: bool,
    cancel_rx: &watch::Receiver<bool>,
) -> Result<String> {
    match op {
        SyncOp::MkDir { path } => {
            if !dry_run {
                // Anything at the target that is not a real directory must be
                // cleared first. A plain file makes create_dir_all fail; worse,
                // a symlink to a directory would be silently followed, letting
                // the copies below write through it to a location outside
                // dst_root. symlink_metadata never follows links, so a dir
                // symlink is caught here. The occupant is always an orphan in
                // this situation (SRC has a directory at this path), and the
                // planner suppresses its Delete op because we clear it here.
                if let Ok(meta) = path.symlink_metadata()
                    && !meta.file_type().is_dir()
                {
                    // On Windows, directory symlinks require remove_dir; try both.
                    if let Err(e) = fs::remove_file(&path) {
                        fs::remove_dir(&path).map_err(|_| e)?;
                    }
                }
                fs::create_dir_all(&path)?;
            }
            Ok(format!("mkdir {}", path.display()))
        }

        SyncOp::RmDir { path } => {
            if !dry_run {
                match fs::remove_dir(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                        // Remaining entries are excluded files or subdirs the plan never
                        // touched. Leave the directory in place rather than destroying
                        // data the user intentionally excluded from sync.
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(format!("rmdir {}", path.display()))
        }

        SyncOp::Delete { path, .. } => {
            if !dry_run {
                // On Windows, directory symlinks require remove_dir; try both.
                if let Err(e) = fs::remove_file(&path) {
                    fs::remove_dir(&path).map_err(|_| e)?;
                }
            }
            Ok(format!("delete {}", path.display()))
        }

        SyncOp::Symlink { target, dst, kind } => {
            if !dry_run {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Remove any existing entry (regular file or symlink) at dst.
                if dst.symlink_metadata().is_ok() {
                    fs::remove_file(&dst).or_else(|_| fs::remove_dir(&dst))?;
                }
                create_link(&target, &dst, kind)?;
            }
            Ok(format!("symlink {} -> {}", dst.display(), target.display()))
        }

        SyncOp::Move { from, to, .. } => {
            if !dry_run {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&from, &to)?;
            }
            Ok(format!("move {} -> {}", from.display(), to.display()))
        }
        SyncOp::CaseRename { from, to, .. } => {
            if !dry_run {
                let fname = from.file_name().unwrap_or_default().to_string_lossy();
                let tmp = from.with_file_name(format!("{}.__dirsync_case__", fname));
                fs::rename(&from, &tmp)?;
                // Put it back under its old name when the second step fails:
                // left at the staging name, the walk (which excludes that
                // suffix) would never see it again.
                if let Err(e) = fs::rename(&tmp, &to) {
                    let _ = fs::rename(&tmp, &from);
                    return Err(e.into());
                }
            }
            Ok(format!(
                "case-rename {} -> {}",
                from.display(),
                to.display()
            ))
        }

        SyncOp::Copy { src, dst, size, .. } => {
            do_copy(&src, &dst, size, "copy", progress, dry_run, cancel_rx).await
        }

        SyncOp::Overwrite { src, dst, size, .. } => {
            do_copy(&src, &dst, size, "overwrite", progress, dry_run, cancel_rx).await
        }

        SyncOp::TouchMtime { src, dst } => {
            if !dry_run {
                let meta = fs::metadata(&src)?;
                let mtime = meta.modified()?;
                with_writable(&dst, || {
                    set_file_mtime(&dst, FileTime::from_system_time(mtime))
                })?;
            }
            Ok(format!("touch-mtime {}", dst.display()))
        }
    }
}

async fn do_copy(
    src: &Path,
    dst: &Path,
    size: u64,
    verb: &str,
    progress: &Arc<ProgressState>,
    dry_run: bool,
    cancel_rx: &watch::Receiver<bool>,
) -> Result<String> {
    let name = src
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    progress.current_file_size.store(size, Ordering::Relaxed);
    progress.current_file_done.store(0, Ordering::Relaxed);
    *progress.current_file.write().unwrap() = Some(name.clone());
    // The GUI lights this file's directory for as long as the copy runs.
    *progress.current_file_dst.write().unwrap() = Some(dst.to_path_buf());
    progress.emit(ProgressEvent::FileStarted {
        name: name.clone(),
        size,
    });

    if !dry_run {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        // Clear current_file on the error path too, or the GUI keeps showing a
        // failed file as in-progress until the next copy starts.
        if let Err(e) = copy_with_progress(src, dst, progress, cancel_rx).await {
            *progress.current_file.write().unwrap() = None;
            *progress.current_file_dst.write().unwrap() = None;
            return Err(e);
        }
    } else {
        // Credit the bytes so dry runs advance the progress bar.
        progress.record_bytes(size);
    }

    progress.emit(ProgressEvent::FileDone { name: name.clone() });
    *progress.current_file.write().unwrap() = None;
    *progress.current_file_dst.write().unwrap() = None;

    Ok(format!(
        "{verb} {} ({})",
        name,
        crate::fmt::fmt_bytes_styled(size, None, Some(crate::fmt::UNIT_MB), crate::fmt::UNIT_TB)
    ))
}

/// Fast copy path for small files (≤ SMALL_FILE bytes).
///
/// Uses std::fs::copy which resolves to copy_file_range(2) on Linux and
/// CopyFileEx on Windows: both are single-syscall kernel copies with no
/// user-space chunk loop. The result is written to a temp file first so the
/// destination is never left in a partial state on failure.
async fn do_copy_small(op: SyncOp, progress: &Arc<ProgressState>, dry_run: bool) -> Result<String> {
    let (src, dst, size, verb) = match op {
        SyncOp::Copy { src, dst, size, .. } => (src, dst, size, "copy"),
        SyncOp::Overwrite { src, dst, size, .. } => (src, dst, size, "overwrite"),
        _ => unreachable!(),
    };
    let name = src
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    if !dry_run {
        tokio::task::spawn_blocking({
            let src = src.clone();
            let dst = dst.clone();
            move || -> Result<()> {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                stage_and_commit(&src, &dst, |tmp| {
                    fs::copy(&src, tmp)?;
                    Ok(())
                })
            }
        })
        .await??;
    }
    // Account for transferred bytes in the overall progress bar. Credited in
    // dry-run too, or the bar sits near zero for a copy-heavy dry run.
    progress.record_bytes(size);

    Ok(format!(
        "{verb} {} ({})",
        name,
        crate::fmt::fmt_bytes_styled(size, None, Some(crate::fmt::UNIT_MB), crate::fmt::UNIT_TB)
    ))
}

async fn copy_with_progress(
    src: &Path,
    dst: &Path,
    progress: &Arc<ProgressState>,
    cancel_rx: &watch::Receiver<bool>,
) -> Result<()> {
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    let progress = progress.clone();
    let cancel_rx = cancel_rx.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        stage_and_commit(&src, &dst, |tmp| {
            let mut src_file = fs::File::open(&src)?;
            let mut dst_file = fs::File::create(tmp)?;

            let mut buf = vec![0u8; COPY_BUF];
            let mut written = 0u64;
            let mut last_event = Instant::now();
            const EVENT_INTERVAL: Duration = Duration::from_millis(100);

            loop {
                // A cancel must interrupt a large file mid-copy: between ops
                // is far too coarse when a single file can take minutes. The
                // error path below removes the staging file.
                if *cancel_rx.borrow() {
                    anyhow::bail!("cancelled");
                }
                let n = src_file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                dst_file.write_all(&buf[..n])?;
                written += n as u64;
                progress.record_bytes(n as u64);
                progress.current_file_done.store(written, Ordering::Relaxed);
                // Rate-limit events to ~10/s so the broadcast channel never floods.
                if last_event.elapsed() >= EVENT_INTERVAL {
                    progress.emit(ProgressEvent::FileProgress {
                        done_bytes: written,
                    });
                    last_event = Instant::now();
                }
            }
            // Durable before the rename publishes it: after a power loss a
            // renamed-but-unflushed file can read back zero-filled while
            // already carrying the source's mtime, which the fast path would
            // then call Identical forever. Small files skip this: one flush
            // per file would dominate a run of thousands of them.
            dst_file.sync_all()?;
            Ok(())
        })
    })
    .await??;

    Ok(())
}

#[cfg(unix)]
fn create_link(target: &Path, link: &Path, _kind: LinkKind) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_link(target: &Path, link: &Path, kind: LinkKind) -> std::io::Result<()> {
    // Windows fixes a link's file/dir nature at creation, and CreateSymbolicLink
    // never checks the target: guessing "file first" turned every directory
    // link into a file link that cannot be opened as a directory.
    match kind {
        LinkKind::File => std::os::windows::fs::symlink_file(target, link),
        LinkKind::Dir => std::os::windows::fs::symlink_dir(target, link),
        LinkKind::Junction => junction::create(target, link),
    }
}

#[cfg(not(any(unix, windows)))]
fn create_link(_target: &Path, _link: &Path, _kind: LinkKind) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks not supported on this platform",
    ))
}

/// NTFS junctions (mount-point reparse points). std reports them as
/// directory symlinks, but recreating one as a symlink needs
/// SeCreateSymbolicLinkPrivilege, which ordinary users lack: every run
/// failed with os error 1314 and the junction never arrived.
#[cfg(windows)]
pub(crate) mod junction {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileAttributeTagInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows::core::PCWSTR;

    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const GENERIC_WRITE: u32 = 0x4000_0000;

    /// Open the reparse point itself, never what it points at.
    fn open(path: &Path, access: u32) -> windows::core::Result<HANDLE> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            CreateFileW(
                PCWSTR::from_raw(wide.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
    }

    pub(crate) fn is_junction(path: &Path) -> bool {
        let Ok(handle) = open(path, 0) else {
            return false;
        };
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileAttributeTagInfo,
                (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        }
        .is_ok();
        unsafe {
            let _ = CloseHandle(handle);
        }
        ok && info.ReparseTag == IO_REPARSE_TAG_MOUNT_POINT
    }

    /// Create a junction at `link` pointing at the absolute `target`: an
    /// empty directory turned into a mount-point reparse point.
    pub(crate) fn create(target: &Path, link: &Path) -> std::io::Result<()> {
        // read_link reports a junction's target as `\\?\C:\...`; the reparse
        // data wants the NT form `\??\C:\...` plus a plain display name.
        let target = target.to_string_lossy();
        let plain = target.strip_prefix(r"\\?\").unwrap_or(&target);
        let substitute: Vec<u16> = format!(r"\??\{plain}").encode_utf16().collect();
        let print: Vec<u16> = plain.encode_utf16().collect();

        // REPARSE_DATA_BUFFER, MountPointReparseBuffer variant. Offsets and
        // lengths are in bytes, relative to PathBuffer; both names are
        // NUL-terminated in the buffer but the lengths exclude the NUL.
        let sub_len = (substitute.len() * 2) as u16;
        let print_len = (print.len() * 2) as u16;
        let path_bytes = (substitute.len() + 1 + print.len() + 1) * 2;
        let data_len = (8 + path_bytes) as u16;
        let mut buf: Vec<u8> = Vec::with_capacity(8 + data_len as usize);
        buf.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        for v in [0, sub_len, sub_len + 2, print_len] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        for unit in substitute.iter().chain(&[0]).chain(&print).chain(&[0]) {
            buf.extend_from_slice(&unit.to_le_bytes());
        }

        std::fs::create_dir(link)?;
        let result = open(link, GENERIC_WRITE).and_then(|handle| {
            let r = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_SET_REPARSE_POINT,
                    Some(buf.as_ptr().cast()),
                    buf.len() as u32,
                    None,
                    0,
                    None,
                    None,
                )
            };
            unsafe {
                let _ = CloseHandle(handle);
            }
            r
        });
        if let Err(e) = result {
            let _ = std::fs::remove_dir(link);
            return Err(std::io::Error::other(e));
        }
        Ok(())
    }
}
