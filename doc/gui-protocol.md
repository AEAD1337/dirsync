# GUI Protocol

The GUI is served by an embedded axum HTTP server, default port 7373, bound to `127.0.0.1` only. Every API and WebSocket request passes three checks (`require_session` in `server.rs`):

1. `Host` must be `127.0.0.1:<port>` or `localhost:<port>`, else `403 Forbidden` (DNS rebinding).
2. An `Origin` header, when present, must be `http://127.0.0.1:<port>` or `http://localhost:<port>`, else `403` (browser CSRF).
3. The per-launch session token must match, else `401 Unauthorized`. The server generates 128 random bits at startup and opens the browser at `http://127.0.0.1:<port>/#t=<token>` (the URL is also printed to the console). The page moves the token from the fragment into `sessionStorage` and sends it as an `X-Dirsync-Token` header on every fetch, and as a `token` query parameter on the WebSocket (`/ws?token=<token>`), whose browser API cannot set headers. Headers are trivial to forge for any local process, and loopback is shared by every user of the machine: the token is what keeps another local user from driving this server, which runs as you.

Static assets (the page itself) need no token: the page has to load before it can read the token from its own URL, and the assets contain nothing secret.

---

## HTTP API

All routes are prefixed `/api/v1/`. Request and response bodies are JSON.

### Config

#### `GET /api/v1/config`
Returns the current `AppConfig`.

```json
{
  "port": 7373,
  "theme": "light",
  "exclude_patterns": [],
  "last_src": "/path/to/src/",
  "last_dst": "/path/to/dst/"
}
```

`theme` is one of `"light"` (the default), `"dark"`, `"system"` (follows the OS dark-mode setting live). `last_src` / `last_dst` are `null` when not yet set. Patterns given with `-e` on the command line appear in `exclude_patterns` for the session but are never written to the file.

#### `PUT /api/v1/config`
Partial update. Body: any subset of `port`, `exclude_patterns`, `theme`; absent fields keep their current value. `last_src` / `last_dst` are not accepted: the server records them on every preview, and a whole-config PUT built from a stale client snapshot used to overwrite them. Returns the full merged config, `400` for a port outside 1024-65535, or `500` if the file could not be written.

---

### Preview

#### `POST /api/v1/preview`
Starts an asynchronous preview analysis. Returns `202 Accepted` immediately; results are delivered over WebSocket.

Request:
```json
{
  "src": "/absolute/path/to/src/",
  "dst": "/absolute/path/to/dst/",
  "excludes": ["*.tmp", ".git"]
}
```

Both paths must end with a directory separator. The server validates that both paths exist, are directories, are not system-critical (unless `--yolo`) and are not nested inside each other; a failure returns `400 Bad Request` with the reason as the body, synchronously, with no WebSocket event. `409 Conflict` means a run or another preview is in progress. `last_src` / `last_dst` in the config are updated as a side effect.

On success the server emits `drive_mode` immediately after detecting drive types, then `scan_update` events during the walk, then `plan_ready` when done, and only then the `status_changed` to `idle` (the plan is stored before the status is released). A SRC or DST root that cannot be read fails the preview. A directory *below* the root that cannot be read is logged as a warning: nothing at or below its DST counterpart is deleted or used as a move source. On any other error the server emits `preview_failed` and resets status to `idle`.

#### `GET /api/v1/plan`
Returns the most recently computed plan as a `PlanSummary`. Returns `404` if no preview has been run yet.

```json
{
  "copy_count": 3,
  "move_count": 1,
  "delete_count": 2,
  "overwrite_count": 0,
  "identical_count": 47,
  "symlink_count": 0,
  "total_bytes": 1048576,
  "total_ops": 6,
  "ops": [
    {
      "kind": "copy",
      "rel_path": "photos/2024/img001.jpg",
      "size": 983040,
      "badge": "+",
      "hash": "a3f2..."
    },
    {
      "kind": "move",
      "rel_path": "archive/old.txt",
      "size": 0,
      "badge": "→",
      "from_path": "trash/old.txt"
    },
    {
      "kind": "delete",
      "rel_path": "orphan.txt",
      "size": 1024,
      "badge": "–"
    }
  ]
}
```

`kind` values: `"copy"`, `"overwrite"`, `"move"`, `"dir-rename"`, `"case-rename"`, `"delete"`, `"symlink"`, `"touch"`. A `touch` (badge `~`, `size` 0) is a timestamp correction on an existing DST file; it gets a row because it writes to a user file. `MkDir` and `RmDir` ops are not included in the ops list (they are infrastructure; the GUI does not display them individually): but they **are** counted in `total_ops`, which is the whole plan's op count and therefore the same denominator the progress bar reports as `ops_total`. `total_ops` is consequently `>= ops.length`. `hash` is a hex-encoded SHA-256, present only when it was computed during matching. `from_path` is present only for `move`, `dir-rename`, and `case-rename`. `"case-rename"` is only emitted when the DST filesystem resolves names ignoring case (NTFS, APFS by default, exFAT, most SMB shares: probed at preview time, not assumed from the OS); `size` is always `0` for this kind.

`total_bytes` is the progress-bar denominator: it is the sum of actual file bytes for copy/overwrite ops **plus** a 128 KB virtual token (`OP_TOKEN_BYTES`) per non-copy op (moves, deletes, mkdirs, rmdirs, symlinks, mtime touches). The token ensures all op types advance the progress bar, not just file copies.

---

### Run

#### `POST /api/v1/run`
Starts executing the last computed plan. Returns `202 Accepted`; `409 Conflict` if a sync is already running, if a preview is in progress, or if `src`/`dst` differ from the stored plan's roots (the user edited the paths after previewing); `400 Bad Request` if no plan exists. A real run drops the plan server-side whether it finished or was cancelled: nothing records which ops already ran, so a replay would redo every completed move and delete and fail them against the correct files. Running again requires a new preview. A dry run changes nothing and keeps the plan.

Request:
```json
{ "dry_run": false, "skip_prefixes": ["photos/2023"], "src": "/absolute/path/to/src/", "dst": "/absolute/path/to/dst/" }
```

`src` and `dst` are required and must equal the paths of the preview that produced the plan.

`skip_prefixes` is optional (defaults to `[]`). Each entry is a forward-slash path relative to `dst_root`, as shown in the preview. Write ops at or below any prefix are dropped from the plan before execution and the plan's counts and `total_bytes` are recomputed; `delete` and `rmdir` ops are kept, because skipping a source directory suppresses writes into DST rather than cancelling cleanup of DST orphans. The stored plan is not modified: filtering applies to a clone, so running again without the prefixes needs no new preview.

Progress is delivered over WebSocket. Run completion is signalled by a `status_changed` (and the next `progress_update`) with status `"done"` or `"cancelled"`. A cancel always ends in `"cancelled"`, including one that interrupts the last op, and is not reported as a file error.

---

### Control

#### `POST /api/v1/pause`
Toggles pause. Returns the new pause state:
```json
{ "paused": true }
```

#### `POST /api/v1/cancel`
Cancels the current preview or run. Returns `204 No Content`.

#### `POST /api/v1/shutdown`
Sets the cancel flag (so an in-flight run stops at its next chunk or op), emits a `shutdown` WebSocket event to all clients, waits 200 ms, then stops the server; the process exits once the executor has written its final status. Returns `204 No Content`. The same sequence runs on SIGINT/SIGTERM and when the last client is gone.

The server also shuts itself down when the last WebSocket client disconnects and none reconnects within 5 s (`CLIENT_GRACE` in `ws.rs`). The grace period is what distinguishes a page reload - back within a second - from a closed tab.

---

### File system helpers

#### `POST /api/v1/browse`
Lists directory contents for the path picker.

Request:
```json
{ "path": "/home/user/", "dir_only": true }
```

If `path` does not exist, the server walks up to the nearest readable ancestor. Returns up to 500 entries, sorted directories first then alphabetically. Hidden *files* (names starting with `.`) are skipped, but hidden directories are listed so they remain navigable. `dir_only: true` drops regular files entirely. Entries whose names are not valid Unicode are skipped: they cannot travel through JSON and back as a path the server could open again.

```json
{
  "path": "/home/user",
  "entries": [
    { "name": "Documents", "path": "/home/user/Documents", "is_dir": true },
    { "name": "notes.txt", "path": "/home/user/notes.txt", "is_dir": false }
  ]
}
```

#### `POST /api/v1/complete`
Returns up to 12 directory path completions for the typed prefix. Used for the inline path input autocomplete.

Request: `{ "path": "/home/us" }`  
Response: `{ "completions": ["/home/user", "/home/usr"] }`

Only directories are completed, and unlike `/browse` this endpoint does skip dotted ones. Completions carry no trailing separator. Matching on the final component is case-insensitive. An empty `path` returns the filesystem roots: every existing drive root on Windows (not capped at 12), the entries of `/` elsewhere.

#### `POST /api/v1/stat`
Lightweight existence check.

Request: `{ "path": "/some/path" }`  
Response: `{ "exists": true, "is_dir": true }`

#### `GET /api/v1/system`
Returns platform metadata the frontend needs on startup.

```json
{ "path_sep": "\\", "auto_preview": false }
```

`auto_preview` is `true` when `--gui` was launched with both SRC and DST given as positional args *and* both already resolve to existing directories, and only on the first `GET /system` of the process: it is cleared once read, so a page reload does not re-scan both trees. There is no `--auto-preview` flag: it is derived in `main.rs`. The frontend uses it to fire a preview on load instead of waiting for the user.

---

### Log

#### `GET /api/v1/log`
Returns the in-memory log ring buffer as an ordered array of entries (oldest first). Capped at 2000 entries. The frontend fetches it once at startup and merges live `log_entry` events into it.

```json
[
  { "level": "info",    "message": "3 copies. 1 move. 2 deletes. 3.4 MB to transfer.", "run": 1 },
  { "level": "warning", "message": "Walk error: IO error for operation on C:\\src\\locked: Access is denied. (os error 5)", "run": 1 },
  { "level": "error",   "message": "  C:\\dst\\locked.db: Access is denied. (os error 5)", "run": 1 },
  { "level": "error",   "message": "1 file(s) had errors and were skipped (listed above).", "run": 1 }
]
```

After a run with failures the per-file lines come first and the summary line last: when a burst overruns a slow consumer of the broadcast channel, the *oldest* events are dropped, and the summary is the line that must survive. A consumer that falls behind records a warning entry saying how many events it missed, so the gap is visible.

`level` is one of `"info"`, `"warning"`, `"error"`. `run` is a monotonically increasing integer, incremented once per preview start; it is used to group entries and render separator lines between runs.

---

## WebSocket

Connect to `ws://127.0.0.1:<port>/ws?token=<token>`. The server pushes JSON messages; each has a `"type"` discriminant field. Two delivery mechanisms are used:

- **Polled (every 100 ms):** `progress_update`: always sent while the connection is open, regardless of activity.
- **Event-driven:** all other event types, sent as soon as the underlying condition occurs.

### State machine

```
idle / done / cancelled
 |- POST /preview -> previewing
 |    |- plan_ready, then status idle   (plan embedded in the event; awaiting POST /run)
 |    |- preview_failed -> idle
 |    '- POST /cancel -> idle           (no plan_ready, no preview_failed)
 |
 |- POST /run -> running
 |    |- POST /pause -> paused -> POST /pause -> running
 |    |- POST /cancel -> cancelled      (plan dropped)
 |    '- all ops attempted -> done      (plan dropped)
 |
 '- POST /shutdown -> (server exits)
```

`done` and `cancelled` are terminal: the status stays there until the next preview, which moves through `previewing` back to `idle`.

Status transitions are reflected in the `status` field of every `progress_update` message, and additionally pushed as a dedicated `status_changed` event the moment they happen.

### Event reference

#### `progress_update`
Sent every 100 ms while the WebSocket connection is open. Provides a complete snapshot of current progress; clients should use this as their primary source of truth rather than maintaining local counters.
```json
{
  "type": "progress_update",
  "done_bytes": 524288,
  "total_bytes": 1048576,
  "current_file": "video.mp4",
  "current_file_done": 32768000,
  "current_file_size": 104857600,
  "current_file_pct": 31.25,
  "current_dir": "photos/2024",
  "speed_mbps": 45.3,
  "elapsed_secs": 12,
  "eta_secs": 11,
  "ops_done": 2,
  "ops_total": 6,
  "status": "running"
}
```
`current_file` is `null` when no large-file copy is in progress. `current_dir` is that file's directory relative to `dst_root`, forward-slashed, and is `null` whenever `current_file` is, plus for a file sitting directly in `dst_root` (which has no directory row to mark). The GUI marks that row as active for the whole copy: `ops_completed` implies where the parallel small copies are working, but says nothing during one long file. `eta_secs` is `null` when speed is too low to estimate. `status` mirrors the state machine values: `"idle"`, `"previewing"`, `"running"`, `"paused"`, `"done"`, `"cancelled"`. `total_bytes` includes 128 KB virtual tokens for non-copy ops (see plan `total_bytes` note above); overall progress is always `done_bytes / total_bytes`.

#### `status_changed`
Pushed on every status transition, as it happens.
```json
{ "type": "status_changed", "status": "previewing" }
```
The 100 ms `progress_update` tick only *samples* the status, so a state the engine enters and leaves inside one tick window would never be observed. The frontend needs the `previewing` -> `idle` edge in particular: a cancelled preview emits neither `plan_ready` nor `preview_failed`, so this event is the only signal that it ended. Values are the same set `progress_update.status` uses.

#### `drive_mode`
Emitted immediately after drive detection at the start of a preview, before the walk begins.
```json
{ "type": "drive_mode", "hdd": false }
```
`hdd` mirrors `DriveProfile::serial_copies()`. `hdd: true` means one or both endpoints resolved to HDD, so copies run one at a time; `hdd: false` means both resolved to SSD/unknown and copies run concurrently. It says nothing about the walk, which is always concurrent, nor about hashing, which is decided per endpoint rather than by this single flag. The GUI uses this to update the drive mode badge (`HDD` / `SSD`); the badge resets to `Auto` when a run completes or is cancelled.

#### `scan_update`
Emitted once per side after the directory walk completes during preview.
```json
{ "type": "scan_update", "side": "src", "file_count": 1842 }
```
`side` is `"src"` or `"dst"`.

#### `scan_progress`
Emitted periodically during the preview phase to show what the engine is doing. `path` is the filename most recently processed (may be `null`).
```json
{ "type": "scan_progress", "phase": "hashing", "path": "video.mp4" }
```
`phase` values: `"walking_src"`, `"walking_dst"`, `"hashing"`, `"planning"`.

#### `plan_ready`
Emitted when the preview plan is ready. Carries the full plan inline: no separate `GET /api/v1/plan` call is required (though that endpoint remains available).
```json
{
  "type": "plan_ready",
  "copy_count": 3,
  "move_count": 1,
  "delete_count": 2,
  "overwrite_count": 0,
  "identical_count": 47,
  "symlink_count": 0,
  "total_bytes": 1048576,
  "total_ops": 8,
  "ops": [
    { "kind": "copy", "rel_path": "photos/2024/img001.jpg", "size": 983040, "badge": "+" },
    { "kind": "move", "rel_path": "archive/old.txt", "size": 0, "badge": "→", "from_path": "trash/old.txt" },
    { "kind": "delete", "rel_path": "orphan.txt", "size": 1024, "badge": "–" }
  ]
}
```

#### `error_occurred`
Emitted when an individual file operation fails. The sync continues; errors accumulate in a skip log printed at the end. `path` is relative to `dst_root` with forward slashes, the same form as `rel_path` in the plan and `ops_completed`, so the client marks that row as failed (failed rows stay visible after the run).
```json
{ "type": "error_occurred", "path": "db/locked.db", "message": "Permission denied (os error 13)" }
```

#### `preview_failed`
Emitted when a preview ends in an error other than a cancel (for example an unreadable SRC root).
```json
{ "type": "preview_failed", "message": "cannot read /src: Permission denied (os error 13)" }
```

#### `ops_completed`
Flushed once per 100 ms tick (batched to avoid flooding the browser's JS event loop). Each entry is a destination path relative to `dst_root`, using forward slashes.
```json
{ "type": "ops_completed", "rel_paths": ["photos/2024/img001.jpg", "photos/2024/img002.jpg"] }
```

#### `log_entry`
Emitted whenever a subsystem writes a structured log line (plan summaries, skip-log errors, warnings, etc.). `run` matches the `run` field in `GET /api/v1/log` entries and increments with each preview start.
```json
{ "type": "log_entry", "level": "info", "message": "3 copies. 1 move. 2 deletes. 3.4 MB to transfer.", "run": 1 }
```
`level` values: `"info"`, `"warning"`, `"error"`. Entries are also appended to the ring buffer returned by `GET /api/v1/log`.

#### `shutdown`
Emitted just before the server process exits. Clients should display a notice and can close the tab.
```json
{ "type": "shutdown" }
```
