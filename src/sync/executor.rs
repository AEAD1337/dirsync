use super::planner::{SyncOp, SyncPlan, OP_TOKEN_BYTES};
use crate::error::SkipLog;
use crate::progress::{ProgressEvent, ProgressState, SyncStatus};
use anyhow::Result;
use filetime::{set_file_mtime, FileTime};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Semaphore};
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
        #[cfg(windows)]
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

    // Safety gate: every write target must be inside dst_root.
    // The planner builds all targets from dst_root, but we verify here so that
    // a future planner bug or a crafted CLI invocation can never cause writes
    // into SRC or any other unrelated directory.
    for op in &plan.ops {
        let target = write_target(op);
        if !target.starts_with(&plan.dst_root) {
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
    #[cfg(windows)]
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
            #[cfg(windows)]
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
            SyncOp::Delete { path, .. } => write_targets.iter().any(|t| path.starts_with(t)),
            _ => false,
        })
    };
    let (blocking_write_rmdirs, rmdirs): (Vec<_>, Vec<_>) = if write_targets.is_empty() {
        (vec![], rmdirs)
    } else {
        rmdirs.into_iter().partition(|op| match op {
            SyncOp::RmDir { path } => write_targets.iter().any(|t| path.starts_with(t)),
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
            dir_move_targets.iter().any(|t| path.starts_with(t))
        } else {
            false
        }
    });

    progress.reset(
        planned_bytes + extra_ops as u64 * OP_TOKEN_BYTES,
        planned_ops + extra_ops,
    );

    // Phase 1: MkDir: serial (must precede all writes, very fast)
    for op in mkdirs {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    // Phase 1.5: clear DST directories that sit where a write op will land, by
    // running their own Delete/RmDir ops early (deepest-first order preserved
    // from the planner). A non-empty directory left by excluded content still
    // survives RmDir, and the write then fails with a clear error rather than
    // silently destroying data the user excluded.
    for op in blocking_write_deletes
        .into_iter()
        .chain(blocking_write_rmdirs)
    {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

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

    for op in blocking_deletes {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    let moves: Vec<_> = sorted_file_moves.into_iter().chain(dir_moves).collect();

    for op in moves {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    // Phase 2.1: MkDirs inside renamed subtrees: deferred until after the
    // dir moves that create their parents (see partition above).
    for op in post_move_mkdirs {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    // Phase 2.5: CaseRenames: serial; two-step rename to force case update on NTFS.
    // Dir case renames run first so their children resolve correctly in Phase 3.
    #[cfg(windows)]
    {
        let (dir_case_renames, file_case_renames): (Vec<_>, Vec<_>) = case_renames
            .into_iter()
            .partition(|op| matches!(op, SyncOp::CaseRename { is_dir: true, .. }));
        for op in dir_case_renames.into_iter().chain(file_case_renames) {
            if *cancel_rx.borrow() {
                return set_status(skip_log, SyncStatus::Cancelled, &progress);
            }
            wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
            run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
        }
    }

    // Phase 3: Symlinks: serial, near-instant (no data to copy).
    for op in symlinks {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    // Phase 3a: small copies.
    // Default: concurrent up to COPY_JOBS workers (fast path via fs::copy).
    // HDD mode: serial: no concurrent seeks on the same physical spindle.
    let (small_copies, large_copies): (Vec<_>, Vec<_>) = copies.into_iter().partition(|op| {
        matches!(op, SyncOp::Copy { size, .. } | SyncOp::Overwrite { size, .. } if *size <= SMALL_FILE)
    });

    if opts.hdd {
        // Serial path: reuse run_one so the progress accounting is identical.
        for op in small_copies {
            if *cancel_rx.borrow() {
                return set_status(skip_log, SyncStatus::Cancelled, &progress);
            }
            wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
            run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
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
    for op in large_copies {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    // Phase 3c: TouchMtime: serial, cheap metadata-only writes.
    for op in touches {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    // Phase 4: Deletes: serial
    for op in deletes {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        wait_if_paused(&pause_rx, &cancel_rx, &progress).await;
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    // Phase 5: RmDir: serial (deepest-first order from planner)
    for op in rmdirs {
        if *cancel_rx.borrow() {
            return set_status(skip_log, SyncStatus::Cancelled, &progress);
        }
        run_one(op, &progress, opts, &mut skip_log, &cancel_rx).await;
    }

    set_status(skip_log, SyncStatus::Done, &progress)
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
        if let Some(&j) = from_index.get(&entries[i].1) {
            if j != i {
                in_deg[i] = 1;
                succ[j] = Some(i);
            }
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
                if let Ok(meta) = path.symlink_metadata() {
                    if !meta.file_type().is_dir() {
                        // On Windows, directory symlinks require remove_dir; try both.
                        if let Err(e) = fs::remove_file(&path) {
                            fs::remove_dir(&path).map_err(|_| e)?;
                        }
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

        SyncOp::Symlink { target, dst } => {
            if !dry_run {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Remove any existing entry (regular file or symlink) at dst.
                if dst.symlink_metadata().is_ok() {
                    fs::remove_file(&dst).or_else(|_| fs::remove_dir(&dst))?;
                }
                create_symlink(&target, &dst)?;
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

        #[cfg(windows)]
        SyncOp::CaseRename { from, to, .. } => {
            if !dry_run {
                let fname = from.file_name().unwrap_or_default().to_string_lossy();
                let tmp = from.with_file_name(format!("{}.__dirsync_case__", fname));
                fs::rename(&from, &tmp)?;
                fs::rename(&tmp, &to)?;
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
                set_file_mtime(&dst, FileTime::from_system_time(mtime))?;
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
            return Err(e);
        }
    } else {
        // Credit the bytes so dry runs advance the progress bar.
        progress.record_bytes(size);
    }

    progress.emit(ProgressEvent::FileDone { name: name.clone() });
    *progress.current_file.write().unwrap() = None;

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
                let tmp = tmp_path_for(&dst);
                // Leaving a stale temp file behind on failure litters DST and
                // lets it take part in the *next* run's rename detection.
                let staged = (|| -> Result<()> {
                    fs::copy(&src, &tmp)?;
                    clear_symlink_at(&dst)?;
                    fs::rename(&tmp, &dst)?;
                    Ok(())
                })();
                if staged.is_err() {
                    let _ = fs::remove_file(&tmp);
                }
                staged?;
                if let Ok(meta) = fs::metadata(&src) {
                    if let Ok(mtime) = meta.modified() {
                        let _ = set_file_mtime(&dst, FileTime::from_system_time(mtime));
                    }
                }
                Ok(())
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
        let tmp = tmp_path_for(&dst);
        // Any early exit below must not leave the staging file behind: it
        // litters DST and would take part in the next run's rename detection.
        let staged = (|| -> Result<()> {
            let mut src_file = fs::File::open(&src)?;
            let mut dst_file = fs::File::create(&tmp)?;

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
            drop(dst_file);
            clear_symlink_at(&dst)?;
            fs::rename(&tmp, &dst)?;
            Ok(())
        })();
        if staged.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        staged?;
        // Preserve source mtime on the destination.
        if let Ok(meta) = fs::metadata(&src) {
            if let Ok(mtime) = meta.modified() {
                let ft = FileTime::from_system_time(mtime);
                let _ = set_file_mtime(&dst, ft);
            }
        }
        Ok(())
    })
    .await??;

    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    // Windows needs separate APIs for file vs directory symlinks; try file first.
    std::os::windows::fs::symlink_file(target, link)
        .or_else(|_| std::os::windows::fs::symlink_dir(target, link))
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks not supported on this platform",
    ))
}
