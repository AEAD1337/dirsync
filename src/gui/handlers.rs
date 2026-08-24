use crate::config::AppConfig;
use crate::gui::state::AppState;
use crate::progress::LogLevel;
use crate::sync::SyncEngine;
use crate::sync::planner::SyncPlan;
use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

// ---------- Config ----------

pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<AppConfig> {
    Json(state.config.read().unwrap().clone())
}

pub async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AppConfig>,
) -> Result<Json<AppConfig>, (StatusCode, String)> {
    if let Err(e) = crate::config::validate_port(body.port) {
        return Err((StatusCode::BAD_REQUEST, e));
    }
    body.save()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    *state.config.write().unwrap() = body.clone();
    Ok(Json(body))
}

// ---------- Preview ----------

#[derive(Deserialize)]
pub struct PreviewRequest {
    pub src: String,
    pub dst: String,
    pub excludes: Vec<String>,
}

#[derive(Serialize)]
pub struct PlanSummary {
    pub copy_count: usize,
    pub move_count: usize,
    pub delete_count: usize,
    pub overwrite_count: usize,
    pub identical_count: usize,
    pub symlink_count: usize,
    pub total_bytes: u64,
    pub total_ops: usize,
    pub ops: Vec<OpEntry>,
    pub src_dir_sizes: std::collections::HashMap<String, u64>,
}

#[derive(Serialize, Clone)]
pub struct OpEntry {
    pub kind: String,
    pub rel_path: String,
    pub size: u64,
    pub badge: String,
    /// Hex-encoded SHA-256 of the source file, if it was computed during matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// For move ops: the old relative path (forward-slash, relative to dst_root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_path: Option<String>,
}

/// GUI log style: fixed 1 decimal, capped at GB.
fn format_bytes(bytes: u64) -> String {
    crate::fmt::fmt_bytes_styled(bytes, Some(1), None, crate::fmt::UNIT_GB)
}

fn plan_log_message(plan: &crate::sync::planner::SyncPlan) -> String {
    use crate::fmt::fmt_count;
    if plan.is_noop() {
        return "Nothing to do.".to_owned();
    }
    let mut parts: Vec<String> = Vec::new();
    if plan.copy_count > 0 {
        parts.push(format!(
            "{} cop{}",
            fmt_count(plan.copy_count),
            if plan.copy_count == 1 { "y" } else { "ies" }
        ));
    }
    if plan.overwrite_count > 0 {
        parts.push(format!(
            "{} overwrite{}",
            fmt_count(plan.overwrite_count),
            if plan.overwrite_count == 1 { "" } else { "s" }
        ));
    }
    if plan.move_count > 0 {
        parts.push(format!(
            "{} move{}",
            fmt_count(plan.move_count),
            if plan.move_count == 1 { "" } else { "s" }
        ));
    }
    if plan.delete_count > 0 {
        parts.push(format!(
            "{} delete{}",
            fmt_count(plan.delete_count),
            if plan.delete_count == 1 { "" } else { "s" }
        ));
    }
    if plan.symlink_count > 0 {
        parts.push(format!(
            "{} symlink{}",
            fmt_count(plan.symlink_count),
            if plan.symlink_count == 1 { "" } else { "s" }
        ));
    }
    if plan.touch_count > 0 {
        parts.push(format!(
            "{} timestamp update{}",
            fmt_count(plan.touch_count),
            if plan.touch_count == 1 { "" } else { "s" }
        ));
    }
    let line = if parts.is_empty() {
        // MkDir/RmDir-only plans still execute work; never claim "Nothing to do."
        format!("{} ops", fmt_count(plan.ops.len()))
    } else {
        parts.join(". ")
    };
    if plan.total_bytes > 0 {
        format!("{line}: {}", format_bytes(plan.total_bytes))
    } else {
        line
    }
}

pub async fn post_preview(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PreviewRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    // Validate paths before touching any state or spawning work.
    for (label, raw) in [("SRC", &body.src), ("DST", &body.dst)] {
        if !raw.ends_with('/') && !raw.ends_with('\\') {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "{label} path must end with a directory separator ('/' or '\\'). Got: {raw}"
                ),
            ));
        }
    }
    let src = PathBuf::from(&body.src);
    let dst = PathBuf::from(&body.dst);
    // Canonicalizes before the security checks so that `..` traversal forms
    // (e.g. `C:\Users\..\Windows\`) are resolved before `is_system_critical`
    // does its prefix matching, then rejects nested SRC/DST pairs. Shared with
    // CLI mode: see `crate::paths`.
    crate::paths::validate_endpoints(&src, &dst, state.yolo)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    // Claim Previewing under the same write-lock discipline as post_run's run
    // claim. Without it a preview started mid-run reset the executor's shared
    // pause/cancel channels (silently resuming a paused run or erasing a
    // pending cancel), racing previews could overwrite last_plan after a newer
    // plan was confirmed, and a preview finishing mid-run wrote Idle over
    // Running: letting a second executor start. Serializing previews and
    // rejecting them during runs closes all three.
    {
        let mut status = state.progress.status.write().unwrap();
        match *status {
            crate::progress::SyncStatus::Running | crate::progress::SyncStatus::Paused => {
                return Err((
                    StatusCode::CONFLICT,
                    "A sync is running: cannot preview".into(),
                ));
            }
            crate::progress::SyncStatus::Previewing => {
                return Err((
                    StatusCode::CONFLICT,
                    "A preview is already in progress".into(),
                ));
            }
            _ => {}
        }
        *status = crate::progress::SyncStatus::Previewing;
    }

    let mut config = state.config.read().unwrap().clone();
    config = config.with_extra_excludes(body.excludes);

    // Save last used paths
    {
        let mut cfg = state.config.write().unwrap();
        cfg.last_src = Some(src.clone());
        cfg.last_dst = Some(dst.clone());
        let _ = cfg.save();
    }

    // Safe now: the claim above guarantees no executor holds these channels.
    state.reset_control();
    let progress = state.progress.clone();
    let cancel_rx = state.cancel_rx();
    progress.next_run();

    tokio::spawn(async move {
        // Drive detection happens inside preview(); it emits the DriveMode
        // event and log line through the shared progress channel.
        let engine = SyncEngine::new(src, dst, Arc::new(config));
        match engine
            .preview(Some(progress.clone()), Some(cancel_rx))
            .await
        {
            Ok(plan) => {
                // Store the plan BEFORE emitting PlanReady so the WS handler
                // can read it without a race when it reacts to the event.
                let log_msg = plan_log_message(&plan);
                *state.last_plan.write().unwrap() = Some(plan);
                progress.emit(crate::progress::ProgressEvent::PlanReady);
                progress.emit_log(LogLevel::Info, log_msg);
            }
            Err(e) => {
                if e.to_string() == "cancelled" {
                    // preview() already released the Previewing claim; without
                    // a log line a cancelled preview leaves no trace at all.
                    progress.emit_log(LogLevel::Info, "Preview cancelled.".to_owned());
                } else {
                    progress.emit(crate::progress::ProgressEvent::FileError {
                        name: "preview".into(),
                        message: e.to_string(),
                    });
                    // Release the preview claim so the frontend clears the
                    // scanning indicator: compare-and-set, never a blind
                    // write that could clobber another actor's status.
                    {
                        let mut status = progress.status.write().unwrap();
                        if *status == crate::progress::SyncStatus::Previewing {
                            *status = crate::progress::SyncStatus::Idle;
                        } else {
                            return;
                        }
                    }
                    progress.emit(crate::progress::ProgressEvent::StatusChanged {
                        status: crate::progress::SyncStatus::Idle,
                    });
                }
            }
        }
    });

    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({}))))
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn plan_to_summary(plan: &SyncPlan) -> PlanSummary {
    use crate::sync::planner::SyncOp;

    // Strip the DST root prefix and normalise to forward slashes for the frontend.
    let rel = |p: &std::path::Path| -> String { crate::paths::rel_to_root(p, &plan.dst_root) };

    let ops: Vec<OpEntry> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            SyncOp::Copy {
                dst, size, hash, ..
            } => Some(OpEntry {
                kind: "copy".into(),
                rel_path: rel(dst),
                size: *size,
                badge: "+".into(),
                hash: hash.as_ref().map(hex_encode),
                from_path: None,
            }),
            SyncOp::Overwrite {
                dst, size, hash, ..
            } => Some(OpEntry {
                kind: "overwrite".into(),
                rel_path: rel(dst),
                size: *size,
                badge: "↻".into(),
                hash: hash.as_ref().map(hex_encode),
                from_path: None,
            }),
            SyncOp::Move {
                from,
                to,
                is_dir: false,
            } => Some(OpEntry {
                kind: "move".into(),
                rel_path: rel(to),
                size: 0,
                badge: "→".into(),
                hash: None,
                from_path: Some(rel(from)),
            }),
            SyncOp::Move {
                from,
                to,
                is_dir: true,
            } => Some(OpEntry {
                kind: "dir-rename".into(),
                rel_path: rel(to),
                size: 0,
                badge: "→".into(),
                hash: None,
                from_path: Some(rel(from)),
            }),
            SyncOp::Delete { path, size } => Some(OpEntry {
                kind: "delete".into(),
                rel_path: rel(path),
                size: *size,
                badge: "–".into(),
                hash: None,
                from_path: None,
            }),
            SyncOp::Symlink { dst, .. } => Some(OpEntry {
                kind: "symlink".into(),
                rel_path: rel(dst),
                size: 0,
                badge: "⇢".into(),
                hash: None,
                from_path: None,
            }),
            #[cfg(windows)]
            SyncOp::CaseRename { from, to, .. } => Some(OpEntry {
                kind: "case-rename".into(),
                rel_path: rel(to),
                size: 0,
                badge: "→".into(),
                hash: None,
                from_path: Some(rel(from)),
            }),
            _ => None,
        })
        .collect();

    // Count every op in the plan, not just the ones with a display row:
    // this is the same denominator the progress bar reports as ops_total.
    let total_ops = plan.ops.len();
    PlanSummary {
        copy_count: plan.copy_count,
        move_count: plan.move_count,
        delete_count: plan.delete_count,
        overwrite_count: plan.overwrite_count,
        identical_count: plan.identical_count,
        symlink_count: plan.symlink_count,
        total_bytes: plan.total_bytes,
        total_ops,
        ops,
        src_dir_sizes: plan.src_dir_sizes.clone(),
    }
}

// ---------- Run ----------

#[derive(Deserialize)]
pub struct RunRequest {
    pub dry_run: bool,
    /// Directories the user chose to skip in the preview, as forward-slash
    /// paths relative to `dst_root`. Write ops at or below these are dropped
    /// from the plan before execution: without this the UI's "Skip this
    /// directory" only hid rows while the files were copied anyway.
    #[serde(default)]
    pub skip_prefixes: Vec<String>,
    /// The SRC/DST the client believes it is running. Must match the stored
    /// plan's roots: without this, editing the path inputs after a preview
    /// left Run enabled and executing the *old* plan (deletes included)
    /// against a destination the UI no longer showed.
    pub src: String,
    pub dst: String,
}

pub async fn post_run(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RunRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Claim the run under a single write lock. Checking with a read lock and
    // letting `execute` set Running later leaves a window in which two POSTs
    // both pass the guard and start executors over the same plan. Previewing
    // is rejected too: a preview finishing mid-run would blindly reset the
    // status, and running a plan while a newer preview is being computed is
    // exactly the stale-plan hazard the src/dst check below also guards.
    {
        let mut status = state.progress.status.write().unwrap();
        match *status {
            crate::progress::SyncStatus::Running | crate::progress::SyncStatus::Paused => {
                return Err((StatusCode::CONFLICT, "A sync is already running".into()));
            }
            crate::progress::SyncStatus::Previewing => {
                return Err((
                    StatusCode::CONFLICT,
                    "A preview is in progress: wait for it to finish".into(),
                ));
            }
            _ => {}
        }
        *status = crate::progress::SyncStatus::Running;
    }
    // From here on, any early return must release the claim.
    let release_claim = || {
        *state.progress.status.write().unwrap() = crate::progress::SyncStatus::Idle;
    };

    let plan = {
        let guard = state.last_plan.read().unwrap();
        match guard.clone() {
            Some(p) => p,
            None => {
                release_claim();
                return Err((StatusCode::BAD_REQUEST, "No preview computed yet".into()));
            }
        }
    };

    // The stored plan must be the one the user is looking at: reject when the
    // request's endpoints differ from the plan's roots.
    if std::path::Path::new(&body.src) != plan.src_root
        || std::path::Path::new(&body.dst) != plan.dst_root
    {
        release_claim();
        return Err((
            StatusCode::CONFLICT,
            "SRC/DST changed since the last preview: run a new preview first".into(),
        ));
    }

    let plan = if body.skip_prefixes.is_empty() {
        plan
    } else {
        let before = plan.ops.len();
        let plan = plan.without_skipped(&body.skip_prefixes);
        let dropped = before.saturating_sub(plan.ops.len());
        state.progress.emit_log(
            LogLevel::Info,
            format!(
                "Skipping {} director{}: {} op(s) removed from the plan.",
                crate::fmt::fmt_count(body.skip_prefixes.len()),
                if body.skip_prefixes.len() == 1 {
                    "y"
                } else {
                    "ies"
                },
                crate::fmt::fmt_count(dropped),
            ),
        );
        plan
    };

    let config = state.config.read().unwrap().clone();
    let src = plan.src_root.clone();
    let dst = plan.dst_root.clone();

    state.reset_control();
    // run() takes the drive mode from plan.hdd, resolved at preview time.
    let engine = SyncEngine::new(src, dst, Arc::new(config));
    let progress = state.progress.clone();
    let pause_rx = state.pause_rx();
    let cancel_rx = state.cancel_rx();
    let dry_run = body.dry_run;

    tokio::spawn(async move {
        let skip_log = engine
            .run(plan, progress.clone(), dry_run, pause_rx, cancel_rx)
            .await;
        if dry_run {
            progress.emit_log(LogLevel::Info, "Dry-run: no changes made.".to_owned());
        }
        if !skip_log.is_empty() {
            progress.emit_log(
                LogLevel::Error,
                format!(
                    "{} file(s) had errors and were skipped:",
                    crate::fmt::fmt_count(skip_log.iter().count())
                ),
            );
            for e in skip_log.iter() {
                progress.emit_log(
                    LogLevel::Error,
                    format!("  {}: {}", e.path.display(), e.message),
                );
            }
        }
    });

    Ok(StatusCode::ACCEPTED)
}

// ---------- Pause / Cancel ----------

pub async fn post_pause(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    // send_replace, not send: `send` drops the value when no receiver is alive,
    // which would make the response report a state the channel never took.
    let currently_paused = *state.pause_tx.borrow();
    state.pause_tx.send_replace(!currently_paused);
    Json(serde_json::json!({ "paused": !currently_paused }))
}

pub async fn post_cancel(State(state): State<Arc<AppState>>) -> StatusCode {
    state.cancel_tx.send_replace(true);
    StatusCode::NO_CONTENT
}

pub async fn post_shutdown(State(state): State<Arc<AppState>>) -> StatusCode {
    // Notify all WebSocket clients so they can close their tab.
    state
        .progress
        .emit(crate::progress::ProgressEvent::Shutdown);
    // Trigger server shutdown after a short delay to let the WS message fly.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let _ = state.shutdown_tx.send(true);
    });
    StatusCode::NO_CONTENT
}

// ---------- Browse ----------

#[derive(Deserialize)]
pub struct BrowseRequest {
    pub path: PathBuf,
    pub dir_only: bool,
}

#[derive(Serialize)]
pub struct BrowseEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Serialize)]
pub struct BrowseResponse {
    pub path: String,
    pub entries: Vec<BrowseEntry>,
}

pub async fn post_browse(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<BrowseRequest>,
) -> Result<Json<BrowseResponse>, (StatusCode, String)> {
    // Directory listing can block for the full OS network timeout on a dead
    // share: keep it off the async workers that also drive the WebSocket
    // progress stream.
    tokio::task::spawn_blocking(move || browse_blocking(body))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
}

fn browse_blocking(body: BrowseRequest) -> Result<BrowseResponse, (StatusCode, String)> {
    let dir = if body.path.as_os_str().is_empty() {
        // Default to home dir or root
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    } else {
        body.path
    };

    // Walk up to the nearest ancestor we can actually read, so Browse opens
    // somewhere useful when the stored path no longer exists.
    let (dir, read_dir) = {
        let mut d = dir.clone();
        loop {
            match std::fs::read_dir(&d) {
                Ok(rd) => break (d, rd),
                Err(_) => match d.parent().map(|p| p.to_path_buf()) {
                    Some(parent) if !parent.as_os_str().is_empty() && parent != d => {
                        d = parent;
                    }
                    _ => {
                        // Last resort: home dir
                        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
                        let rd = std::fs::read_dir(&home)
                            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                        break (home, rd);
                    }
                },
            }
        }
    };

    let mut entries = Vec::new();

    for entry in read_dir.flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if body.dir_only && !meta.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !meta.is_dir() {
            continue; // skip hidden files but show hidden directories for navigation
        }
        entries.push(BrowseEntry {
            name,
            path: entry.path(),
            is_dir: meta.is_dir(),
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    entries.truncate(500);

    Ok(BrowseResponse {
        path: crate::paths::to_slash(&dir),
        entries,
    })
}

// ---------- Complete ----------

#[derive(Deserialize)]
pub struct CompleteRequest {
    pub path: String,
}

#[derive(Serialize)]
pub struct CompleteResponse {
    pub completions: Vec<String>,
}

pub async fn post_complete(Json(body): Json<CompleteRequest>) -> Json<CompleteResponse> {
    // Fired per keystroke; a dead network path must not pile up blocked
    // async workers (see post_browse).
    let completions = tokio::task::spawn_blocking(move || complete_path(&body.path))
        .await
        .unwrap_or_default();
    Json(CompleteResponse { completions })
}

fn complete_path(input: &str) -> Vec<String> {
    if input.is_empty() {
        return list_root_entries();
    }

    let path = std::path::Path::new(input);

    // If input ends with a separator or names an existing directory, list its children.
    let ends_with_sep = input.ends_with('/') || input.ends_with('\\');
    if ends_with_sep || (path.is_dir() && !path.is_relative()) {
        return list_dir_names(path, "");
    }

    // Otherwise the last component is an incomplete name; filter siblings by it.
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => return list_root_entries(),
    };
    let prefix = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    list_dir_names(parent, &prefix)
}

fn list_dir_names(dir: &std::path::Path, prefix: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut results: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_dir() {
                return None;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            if !prefix.is_empty() && !name.to_lowercase().starts_with(prefix) {
                return None;
            }
            Some(e.path().to_string_lossy().into_owned())
        })
        .collect();
    results.sort();
    results.truncate(12);
    results
}

fn list_root_entries() -> Vec<String> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .filter_map(|c| {
                let drive = format!("{}:\\", c as char);
                if std::path::Path::new(&drive).exists() {
                    Some(drive)
                } else {
                    None
                }
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        list_dir_names(std::path::Path::new("/"), "")
    }
}

// ---------- System ----------

pub async fn get_system(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "path_sep": std::path::MAIN_SEPARATOR.to_string(),
        "auto_preview": state.auto_preview,
    }))
}

// ---------- Stat ----------

#[derive(Deserialize)]
pub struct StatRequest {
    pub path: PathBuf,
}

#[derive(Serialize)]
pub struct StatResponse {
    pub exists: bool,
    pub is_dir: bool,
}

/// Lightweight existence check for a path: used by the frontend to colour
/// the SRC/DST inputs without opening a full directory listing.
pub async fn post_stat(Json(body): Json<StatRequest>) -> Json<StatResponse> {
    // Same spawn_blocking rationale as post_browse: a stat on a dead share
    // blocks for the OS network timeout.
    let meta = tokio::task::spawn_blocking(move || std::fs::metadata(&body.path)).await;
    match meta {
        Ok(Ok(meta)) => Json(StatResponse {
            exists: true,
            is_dir: meta.is_dir(),
        }),
        _ => Json(StatResponse {
            exists: false,
            is_dir: false,
        }),
    }
}

// ---------- Plan ----------

pub async fn get_plan(
    State(state): State<Arc<AppState>>,
) -> Result<Json<PlanSummary>, (StatusCode, String)> {
    let guard = state.last_plan.read().unwrap();
    let plan = guard
        .as_ref()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "No plan available".into()))?;
    Ok(Json(plan_to_summary(plan)))
}

// ---------- Log ----------

pub async fn get_log(State(state): State<Arc<AppState>>) -> Json<Vec<crate::progress::LogEntry>> {
    let buf = state.log_buffer.lock().unwrap();
    Json(buf.iter().cloned().collect())
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    // format_bytes is a thin wrapper over crate::fmt::fmt_bytes_styled; the
    // formatting tests live in crate::fmt.

    // Path-validation tests live in `crate::paths` alongside the code they cover.

    // --- plan_log_message ---

    fn make_plan(
        copy_count: usize,
        move_count: usize,
        delete_count: usize,
        overwrite_count: usize,
        symlink_count: usize,
        total_bytes: u64,
    ) -> crate::sync::planner::SyncPlan {
        use crate::sync::planner::SyncOp;
        // The op list must be consistent with the counts: is_noop() (and thus
        // "Nothing to do.") is derived from `ops`, not from the counters.
        let mut ops: Vec<SyncOp> = Vec::new();
        let p = |i: usize| std::path::PathBuf::from(format!("/dst/f{i}"));
        for i in 0..copy_count {
            ops.push(SyncOp::Copy {
                src: p(i),
                dst: p(i),
                size: 0,
                hash: None,
            });
        }
        for i in 0..move_count {
            ops.push(SyncOp::Move {
                from: p(i),
                to: p(i),
                is_dir: false,
            });
        }
        for i in 0..delete_count {
            ops.push(SyncOp::Delete {
                path: p(i),
                size: 0,
            });
        }
        for i in 0..overwrite_count {
            ops.push(SyncOp::Overwrite {
                src: p(i),
                dst: p(i),
                size: 0,
                hash: None,
            });
        }
        for i in 0..symlink_count {
            ops.push(SyncOp::Symlink {
                target: p(i),
                dst: p(i),
            });
        }
        crate::sync::planner::SyncPlan {
            ops,
            total_bytes,
            copy_count,
            move_count,
            delete_count,
            overwrite_count,
            identical_count: 0,
            touch_count: 0,
            symlink_count,
            src_root: std::path::PathBuf::from("/src"),
            dst_root: std::path::PathBuf::from("/dst"),
            hdd: false,
            src_dir_sizes: std::collections::HashMap::new(),
            dir_blocked_targets: vec![],
        }
    }

    #[test]
    fn test_plan_log_message_nothing_to_do() {
        let plan = make_plan(0, 0, 0, 0, 0, 0);
        assert_eq!(plan_log_message(&plan), "Nothing to do.");
    }

    #[test]
    fn test_plan_log_message_symlink_only_not_nothing() {
        let plan = make_plan(0, 0, 0, 0, 1, 0);
        let msg = plan_log_message(&plan);
        assert_ne!(
            msg, "Nothing to do.",
            "symlink-only plan should not say Nothing to do."
        );
        assert!(msg.contains("symlink"), "expected 'symlink' in: {msg}");
    }

    #[test]
    fn test_plan_log_message_symlink_plural() {
        let plan = make_plan(0, 0, 0, 0, 3, 0);
        let msg = plan_log_message(&plan);
        assert!(msg.contains("3 symlinks"), "expected plural: {msg}");
    }

    #[test]
    fn test_plan_log_message_symlink_singular() {
        let plan = make_plan(0, 0, 0, 0, 1, 0);
        let msg = plan_log_message(&plan);
        assert!(msg.contains("1 symlink"), "expected singular: {msg}");
        assert!(!msg.contains("symlinks"), "should not be plural: {msg}");
    }

    #[test]
    fn test_plan_log_message_mixed_ops() {
        let plan = make_plan(2, 1, 1, 0, 2, 1024 * 1024);
        let msg = plan_log_message(&plan);
        assert!(msg.contains("2 copies"), "copies: {msg}");
        assert!(msg.contains("1 move"), "move: {msg}");
        assert!(msg.contains("1 delete"), "delete: {msg}");
        assert!(msg.contains("2 symlinks"), "symlinks: {msg}");
        assert!(msg.contains("MB"), "bytes: {msg}");
    }
}
