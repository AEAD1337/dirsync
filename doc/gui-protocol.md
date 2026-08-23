# GUI Protocol

The GUI is served by an embedded axum HTTP server, default port 7373, bound to `127.0.0.1` only. All API and WebSocket routes require a same-origin `Origin` header: requests from other origins receive `403 Forbidden`.

---

## HTTP API

All routes are prefixed `/api/v1/`. Request and response bodies are JSON.

### Config

#### `GET /api/v1/config`
Returns the current `AppConfig`.

```json
{
  "port": 7373,
  "theme": "system",
  "exclude_patterns": [],
  "last_src": "/path/to/src/",
  "last_dst": "/path/to/dst/"
}
```

`theme` is one of `"light"`, `"dark"`, `"system"`. `last_src` / `last_dst` are `null` when not yet set.

#### `PUT /api/v1/config`
Saves a new config. Body: same shape as the GET response. Returns the saved config or `500`.

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

Both paths must end with a directory separator. The server validates that both paths exist, are directories, and are not nested inside each other. `last_src` / `last_dst` in the config are updated as a side effect.

On success the server emits `drive_mode` immediately after detecting drive types, then `scan_update` events during the walk, then `plan_ready` when done. On error it emits an `error_occurred` event and resets status to `idle`.

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
  "src_dir_sizes": { "photos": 983040, "photos/2024": 983040 },
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

`kind` values: `"copy"`, `"overwrite"`, `"move"`, `"dir-rename"`, `"case-rename"`, `"delete"`, `"symlink"`. `MkDir`, `RmDir`, and `TouchMtime` ops are not included in the ops list (they are infrastructure; the GUI does not display them individually): but they **are** counted in `total_ops`, which is the whole plan's op count and therefore the same denominator the progress bar reports as `ops_total`. `total_ops` is consequently `>= ops.length`. `hash` is a hex-encoded SHA-256, present only when it was computed during matching. `from_path` is present only for `move`, `dir-rename`, and `case-rename`. `"case-rename"` is only emitted on Windows (NTFS case-only renames); `size` is always `0` for this kind.

`total_bytes` is the progress-bar denominator: it is the sum of actual file bytes for copy/overwrite ops **plus** an 8 KB virtual token per non-copy op (moves, deletes, mkdirs, rmdirs, symlinks, mtime touches). The token ensures all op types advance the progress bar, not just file copies.

---

### Run

#### `POST /api/v1/run`
Starts executing the last computed plan. Returns `202 Accepted`, or `409 Conflict` if a sync is already running, or `400 Bad Request` if no plan exists.

Request:
```json
{ "dry_run": false, "skip_prefixes": ["photos/2023"] }
```

`skip_prefixes` is optional (defaults to `[]`). Each entry is a forward-slash path relative to `dst_root`, as shown in the preview. Write ops at or below any prefix are dropped from the plan before execution and the plan's counts and `total_bytes` are recomputed; `delete` and `rmdir` ops are kept, because skipping a source directory suppresses writes into DST rather than cancelling cleanup of DST orphans. The stored plan is not modified: filtering applies to a clone, so running again without the prefixes needs no new preview.

Progress is delivered over WebSocket. Run completion is signalled by a `progress_update` event where `status` is `"done"` or `"cancelled"`.

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
Emits a `shutdown` WebSocket event to all clients, waits 200 ms, then terminates the server process. Returns `204 No Content`.

The server also shuts itself down when the last WebSocket client disconnects and none reconnects within 5 s (`CLIENT_GRACE` in `ws.rs`). The grace period is what distinguishes a page reload - back within a second - from a closed tab.

---

### File system helpers

#### `POST /api/v1/browse`
Lists directory contents for the path picker.

Request:
```json
{ "path": "/home/user/", "dir_only": true }
```

If `path` does not exist, the server walks up to the nearest readable ancestor. Returns up to 500 entries, sorted directories first then alphabetically. Hidden *files* (names starting with `.`) are skipped, but hidden directories are listed so they remain navigable. `dir_only: true` drops regular files entirely.

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
Response: `{ "completions": ["/home/user/", "/home/usr/"] }`

Only directories are completed, and unlike `/browse` this endpoint does skip dotted ones. Matching on the final component is case-insensitive.

#### `POST /api/v1/stat`
Lightweight existence check.

Request: `{ "path": "/some/path" }`  
Response: `{ "exists": true, "is_dir": true }`

#### `GET /api/v1/system`
Returns platform metadata the frontend needs on startup.

```json
{ "path_sep": "\\", "auto_preview": false }
```

`auto_preview` is `true` when `--gui` was launched with both SRC and DST given as positional args *and* both already resolve to existing directories. There is no `--auto-preview` flag: it is derived in `main.rs`. The frontend uses it to fire a preview on load instead of waiting for the user.

---

### Log

#### `GET /api/v1/log`
Returns the in-memory log ring buffer as an ordered array of entries (oldest first). Capped at 2000 entries. Use this on modal open to populate history; subscribe to the WebSocket `log_entry` event for live updates.

```json
[
  { "level": "info",    "message": "Copied 3 files, moved 1, deleted 2.", "run": 1 },
  { "level": "warning", "message": "Symlink skipped: target outside dst.",  "run": 1 },
  { "level": "error",   "message": "2 file(s) had errors and were skipped:", "run": 1 }
]
```

`level` is one of `"info"`, `"warning"`, `"error"`. `run` is a monotonically increasing integer, incremented once per preview start; it is used to group entries and render separator lines between runs.

---

## WebSocket

Connect to `ws://127.0.0.1:<port>/ws`. The server pushes JSON messages; each has a `"type"` discriminant field. Two delivery mechanisms are used:

- **Polled (every 100 ms):** `progress_update`: always sent while the connection is open, regardless of activity.
- **Event-driven:** all other event types, sent as soon as the underlying condition occurs.

### State machine

```
idle
 ├─ POST /preview → previewing
 │    ├─ plan_ready → idle  (plan embedded in event; awaiting POST /run)
 │    └─ error_occurred → idle
 │
 ├─ POST /run → running
 │    ├─ POST /pause → paused → POST /pause → running
 │    ├─ POST /cancel → cancelled → idle
 │    └─ progress_update(status="done") → idle
 │
 └─ POST /shutdown → (server exits)
```

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
  "speed_mbps": 45.3,
  "elapsed_secs": 12,
  "eta_secs": 11,
  "ops_done": 2,
  "ops_total": 6,
  "status": "running"
}
```
`current_file` is `null` when no large-file copy is in progress. `eta_secs` is `null` when speed is too low to estimate. `status` mirrors the state machine values: `"idle"`, `"previewing"`, `"running"`, `"paused"`, `"done"`, `"cancelled"`. `total_bytes` includes 8 KB virtual tokens for non-copy ops (see plan `total_bytes` note above); overall progress is always `done_bytes / total_bytes`.

#### `status_changed`
Pushed on every status transition, as it happens.
```json
{ "type": "status_changed", "status": "previewing" }
```
The 100 ms `progress_update` tick only *samples* the status, so a state the engine enters and leaves inside one tick window would never be observed. The frontend needs the `previewing` → `idle` edge in particular: a cancelled preview emits neither `plan_ready` nor `error_occurred`, so this event is the only signal that it ended. Values are the same set `progress_update.status` uses.

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
  "src_dir_sizes": { "photos": 983040, "photos/2024": 983040 },
  "ops": [
    { "kind": "copy", "rel_path": "photos/2024/img001.jpg", "size": 983040, "badge": "+" },
    { "kind": "move", "rel_path": "archive/old.txt", "size": 0, "badge": "→", "from_path": "trash/old.txt" },
    { "kind": "delete", "rel_path": "orphan.txt", "size": 1024, "badge": "–" }
  ]
}
```

#### `error_occurred`
Emitted when an individual file operation fails. The sync continues; errors accumulate in a skip log printed at the end.
```json
{ "type": "error_occurred", "path": "locked.db", "message": "Permission denied (os error 13)" }
```

#### `ops_completed`
Flushed once per 100 ms tick (batched to avoid flooding the browser's JS event loop). Each entry is a destination path relative to `dst_root`, using forward slashes.
```json
{ "type": "ops_completed", "rel_paths": ["photos/2024/img001.jpg", "photos/2024/img002.jpg"] }
```

#### `log_entry`
Emitted whenever a subsystem writes a structured log line (plan summaries, skip-log errors, warnings, etc.). `run` matches the `run` field in `GET /api/v1/log` entries and increments with each preview start.
```json
{ "type": "log_entry", "level": "info", "message": "Copied 3 files, moved 1, deleted 2.", "run": 1 }
```
`level` values: `"info"`, `"warning"`, `"error"`. Entries are also appended to the ring buffer returned by `GET /api/v1/log`.

#### `shutdown`
Emitted just before the server process exits. Clients should display a notice and can close the tab.
```json
{ "type": "shutdown" }
```
