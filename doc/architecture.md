# Architecture

dirsync is a one-way directory mirror. All sync logic lives in `src/sync/`; the GUI and CLI are thin shells that invoke the same engine. Both shells validate their SRC/DST pair through `src/paths.rs` before the engine sees it: the guards belong to the operation, not to one front end.

## Pipeline overview

```
Walk (parallel)
  └─ src/sync/walker.rs
        │  Walk { entries: Vec<FileEntry>, errors: Vec<WalkError> }
        ▼
Match
  └─ src/sync/matcher.rs
        │  MatchOutput (matched entries + orphans + renamed dirs)
        ▼
Plan
  └─ src/sync/planner.rs
        │  SyncPlan (ordered Vec<SyncOp> + counters)
        ▼
Execute
  └─ src/sync/executor.rs
        │  SkipLog (files that had errors)
```

`SyncEngine` in `src/sync/mod.rs` owns the SRC/DST root paths, the `AppConfig`, and an optional `DriveProfile` (`src_hdd` / `dst_hdd`) that controls I/O scheduling. When the profile is `None` the engine probes it itself at the start of `preview()`, so no entrypoint can forget to; `with_drives()` (or `with_hdd()`, which fills both flags) overrides that for the CLI and tests. The engine sequences the four stages via `preview()` and `run()`.

---

## Stage 0: Drive detection (`drive.rs`)

Before walking, `drive::probe(src, dst)` classifies both endpoints. The engine calls it itself unless the caller supplied a profile: the CLI probes up front so it can print the result before the walk starts, the GUI lets the engine do it. The detection is platform-specific:

- **Windows**: opens a handle to the volume (`\\.\X:`) and issues `IOCTL_STORAGE_QUERY_PROPERTY` with `StorageDeviceTrimProperty`. `TrimEnabled = true` → SSD; anything else (no TRIM, failed query, UNC path) → HDD. This avoids the seek-penalty IOCTL that sysinfo uses, which misclassifies USB flash drives and virtual/encrypted volumes (VeraCrypt) as HDD.
- **Linux / macOS**: queries sysinfo, which reads `/sys/block/*/queue/rotational` on Linux and IOKit on macOS. Unknown results default to SSD.

`probe` returns a `DriveProfile` carrying one flag per endpoint plus a ready-made log line, which is logged (CLI: `println!`; GUI: `emit_log` and a `drive_mode` event). The flags are kept separate rather than collapsed into one boolean: `serial_copies()` is the only consumer that ORs them, because a copy touches both sides at once. Per-side work - walking, hashing - stays at its own drive's pace.

---

## Stage 1: Walk (`walker.rs`)

Both trees are walked concurrently: the engine drives them with `tokio::join!` over two `spawn_blocking` tasks, unconditionally. Drive type never gates this: each walk reads exactly one endpoint, and SRC and DST are assumed to be separate devices, so the two can never contend for the same spindle. `WalkDir` traverses depth-first with `follow_links(false)`. Regular files and directories are recorded normally. Symlinks are preserved as-is: each symlink becomes a `FileEntry` with `symlink_target` set to the raw link target (not followed); symlinks to directories are not traversed into. For each entry the walker records:

- `rel_path`: path relative to the tree root, used as the identity key for matching
- `abs_path`: used for all I/O
- `size`: bytes (0 for directories and symlinks)
- `mtime`: last-modified timestamp; falls back to `UNIX_EPOCH` if unavailable or for symlinks
- `is_dir`: separates file and directory entries
- `symlink_target`: `Option<PathBuf>`; `Some(target)` for symlinks (raw target, not resolved), `None` for regular files and dirs

**Unreadable paths** are returned next to the entries (`Walk::errors`, relative paths plus messages) instead of being skipped silently. An unreadable *root* is an error: the walk fails rather than reporting an empty tree. After both walks the engine drops every DST entry at or below an unreadable SRC path before matching (`shield_unreadable` in `mod.rs`), so nothing there is deleted or claimed as a move source, and stores all walk errors on the plan (`SyncPlan::walk_errors`) for the CLI to report and fail its exit status on.

**Exclusions** are applied per path component. A pattern matching any single component anywhere in the relative path excludes the entire subtree. Built-in exclusions (`System Volume Information`, `$Recycle.Bin`, etc.) are prepended before user patterns.

---

## Stage 2: Match (`matcher.rs`)

Produces a `MatchResult` for every SRC file, plus an orphan list for DST files with no SRC counterpart.

### Phase 0: Renamed directory detection

A directory is considered renamed when it appears in SRC under a new path but its contents (modelled as `sorted[(path_within_dir, size)]`) match a DST directory's contents exactly. This fingerprint is computed purely from walk metadata: no I/O, no hashing.

Detection processes SRC dirs shallowest-first so parent renames are claimed before child dirs. Once a dir pair is matched, its files are excluded from file-level move detection because the dir-level `Move` op handles them wholesale. DST candidates are indexed by a 64-bit digest of their fingerprint and confirmed against the real fingerprint on a hit, so memory stays linear in the file count.

A `rename_index` maps each known `dst_rel` to its rename record, enabling O(path-depth) effective-path lookups for files inside renamed dirs (`deepest_rename`, shared with the planner; renames can nest, and the deepest one applies).

Before classification the matcher probes whether DST resolves names ignoring case (`dst_is_case_insensitive`: an existing DST name with its letter case flipped must resolve to the same file). The result is carried on `MatchOutput::case_insensitive` for the planner.

### Phase 1: Classify without I/O

Each SRC file is looked up in `dst_by_path` (keyed by effective DST path). Three outcomes drive what gets added to the `needs_hash` set:

| Situation | What gets hashed |
|---|---|
| Same path, same size, mtimes within 3 s | Nothing - fast-path Identical |
| Same path only through a detected dir rename, same size | Both SRC and DST files (the rename pairing trusted sizes alone) |
| Same path, same size, mtimes diverge | Both SRC and DST files |
| Same path, different size | SRC only (will be Overwrite; hash stored for GUI display) |
| No same-path DST file; case-insensitive DST and a path match ignoring case, same size, mtimes diverge | Both SRC and DST files (case-rename with mtime drift) |
| No same-path DST file; case-insensitive DST and a path match ignoring case, different size | SRC only |
| No same-path DST file, size > 0, same-size DST candidates exist | SRC + all same-size DST candidates |
| No same-path DST file, size > 0, no same-size DST candidates | Nothing |
| No same-path DST file, size == 0 | Nothing - zero-byte files are excluded from move detection |

Move candidates exclude DST files that the executor clears before the move phase: one sitting where SRC has a directory, or below a path where SRC has a file or symlink. They become plain orphans instead (see decisions.md, "Executor phase ordering").

On a case-insensitive DST a secondary `dst_by_path_lower` index (keyed by lowercased effective path) is built alongside `dst_by_path`. When a SRC file has no exact-case DST counterpart but the lowercased lookup finds one, that branch handles hashing and skips the move-detection path.

### Phase 2: Hash

`needs_hash` is split by endpoint and the two sides are hashed concurrently via `rayon::join`. Each side then runs at its own drive's pace: strictly serial for a spinning endpoint (one seek stream), `par_iter` across all cores otherwise. So two HDDs still hash simultaneously, and a mixed pair lets the SSD side use every core while the HDD side stays orderly. `fingerprint::hash_file` (see below) is called for each file.

### Phase 3: Finalize matches

A pre-pass marks every DST path that has a same-path SRC counterpart as reserved, preventing those files from being claimed as move sources by other SRC files processed earlier in the loop. On a case-insensitive DST the pre-pass also reserves DST paths that differ only in case, so case-mismatched files are not double-claimed as move sources.

Match results:

| `MatchResult` | Meaning |
|---|---|
| `Identical` | Same path, same size, mtimes within 3 s tolerance - no action needed |
| `IdenticalMtimeDiverged` | Same path, same size, hashes match, but mtime difference exceeds tolerance - touch DST mtime |
| `SamePathDifferentContent` | Same path, different size or different hash - overwrite |
| `MovedFrom(old_path)` | No same-path DST file, but a same-hash same-size DST file exists elsewhere |
| `NewInSrc` | No match anywhere in DST |

`MatchedEntry` carries an optional `case_renamed_from: Option<PathBuf>` field. On a case-insensitive DST, when a SRC file matches a DST file via the case-insensitive index (and there is no exact-case match), `case_renamed_from` is set to the DST file's current rel-path. The planner uses this to emit a `CaseRename` op instead of a `Copy`. `touch_after_move` marks a `MovedFrom` whose DST mtime is outside the tolerance, so the planner adds a `TouchMtime` to the same plan.

---

## Stage 3: Plan (`planner.rs`)

Translates `MatchOutput` into a concrete, ordered `Vec<SyncOp>`:

```rust
pub enum SyncOp {
    MkDir      { path }
    Move       { from, to, is_dir }
    CaseRename { from, to, is_dir }   // case-only rename on a case-insensitive DST (two-step via temp)
    Copy       { src, dst, size, hash }
    Overwrite  { src, dst, size, hash }
    Symlink    { target, dst, kind }  // kind: File | Dir | Junction (Windows needs to know)
    TouchMtime { src, dst }
    Delete     { path, size }
    RmDir      { path }
}
```

The executor partitions the op list into its phases itself (see Stage 4); within a kind, planner order is kept. `RmDir` ops are sorted deepest-first so each `remove_dir` call finds an already-empty directory. Orphan Deletes whose path is also a write target are suppressed (`occupied_dsts`; compared ignoring case on a case-insensitive DST, where `photo.jpg` and `Photo.jpg` are one file).

Counters (`copy_count`, `overwrite_count`, `move_count`, `delete_count`, `identical_count`, `touch_count`, `symlink_count`) and `total_bytes` are accumulated here and exposed in `SyncPlan` for the GUI and CLI summary.

`total_bytes` is the primary progress-bar denominator. It includes:
- actual file bytes for every `Copy` and `Overwrite` op
- a fixed 128 KB virtual token (`OP_TOKEN_BYTES`) for every other op (`Move`, `Delete`, `MkDir`, `RmDir`, `Symlink`, `TouchMtime`, `CaseRename`)

This ensures all operation types advance the overall progress bar, not just file copies.

---

## Stage 4: Execute (`executor.rs`)

A safety gate at the start of `execute()` verifies that every path an op writes or removes, a rename's source included, is inside `dst_root` and contains no `..` component, before any op runs.

Ops are partitioned by type and executed in fixed phase order. Every serial phase goes through `run_serial`, which honours pause and cancel between ops:

1. **MkDir**: serial; must precede all writes. `create_dir_all` is used, so missing parent dirs are never a problem. If a non-directory occupies the target path (type conflict - SRC has a dir, DST has a file or link), it is removed first. MkDirs inside a renamed subtree are deferred to step 5.
2. **Hoisted cleanup**: the Delete/RmDir ops of write targets that DST holds as a directory (`dir_blocked_targets`). A directory that survives (excluded content inside) is reported by name and the writes aimed at it are dropped.
3. **Deletes of files occupying a dir-move target** (ENOTDIR/ERROR_ALREADY_EXISTS protection).
4. **Move (files)**, topologically sorted: if move A would overwrite move B's source, B runs first; cycles are broken by renaming one participant to a temp name (`.<name>.__dirsync_swap_N__`). Then **Move (dirs)**. The two sets are independent (a dir rename needs an identical recursive fingerprint).
5. **MkDir inside renamed subtrees**, after the dir moves that create their parents.
6. **CaseRename**: serial; dirs first, then files. Each rename is a two-step via a temporary name (`<name>.__dirsync_case__`) to force a case-insensitive filesystem to update the stored directory-entry case; a failed second step is undone.
7. **Symlink**: serial, near-instant. Any existing entry at the destination is removed first, then the link is created by its recorded kind: `symlink()` on Unix; `symlink_file` / `symlink_dir` on Windows, or a junction (mount-point reparse point, no privilege needed) for a SRC junction.
8. **Copy / Overwrite (small, up to 1 MB)**: up to 8 concurrent workers via Tokio + Semaphore, or strictly serial when the drive profile reports either endpoint as spinning media (`serial_copies()`), since a copy reads SRC and writes DST in the same operation. Uses `std::fs::copy`, which resolves to `copy_file_range(2)` on Linux and `CopyFileEx` on Windows. Then **large (> 1 MB)**: serial, chunked (256 KB buffer), per-chunk progress events (~10/s), polling cancel per chunk, flushed with `sync_all` before commit. Both go through `stage_and_commit`: written to `<name>.__dirsync_tmp__`, checked that SRC's size and mtime did not change during the copy, given SRC's pre-copy mtime and permissions, then atomically renamed over the target (a readonly target is cleared first on Windows).
9. **TouchMtime**: serial; cheap metadata-only writes (`set_file_mtime`), lifting and restoring a Windows readonly attribute around the write.
10. **Delete**: serial file removals.
11. **RmDir**: serial directory removals, deepest-first. A directory that is not empty (excluded content) is left in place.

The run ends `Cancelled` if the cancel flag is set when the last phase returns (a cancel that interrupted the last op), `Done` otherwise. An op interrupted by a cancel is not a file error.

Every non-Copy/Overwrite op that completes successfully calls `progress.record_bytes(OP_TOKEN_BYTES)` to credit its 128 KB token, keeping `done_bytes / total_bytes` consistent as the single progress metric throughout the run.

---

## Hashing (`fingerprint.rs`)

`hash_file` computes SHA-256. For files up to 1 MB the full content is hashed, read no further than the size the walk recorded (a file that grew since is not read whole). For larger files only the first 512 KB and last 512 KB are read and hashed together. This makes the hash a fast probabilistic identity check rather than a cryptographic guarantee: good enough for sync decisions, and orders of magnitude faster for large media files.

The 512 KB chunk size means two large files with identical head and tail but different middles will collide. This is an accepted trade-off: the probability is negligible for real-world sync scenarios, and the alternative (full hashing) would dominate runtime for large video/image libraries.

---

## Module map

```
src/
  lib.rs               - library root; declares the public modules
  main.rs              - thin entrypoint; CLI arg parsing, then GUI or CLI mode; maps errors to exit codes
  cli.rs               - CLI argument definitions, help text, exit status constants
  cli_ui.rs            - terminal progress display for CLI mode
  completions.rs       - shell completion scripts (bash, zsh, fish, PowerShell)
  config.rs            - AppConfig (serde, platform config dir or DIRSYNC_CONFIG, defaults)
  fmt.rs               - shared byte/count formatting
  drive.rs             - drive-type detection (Windows: TRIM IOCTL; Linux/macOS: sysinfo)
  error.rs             - SkipLog (collects per-file errors without aborting the run)
  paths.rs             - endpoint validation shared by CLI and GUI
                         (canonicalization, system-critical guard, nesting checks)
  progress.rs          - ProgressState + ProgressEvent + SyncStatus
  sync/
    mod.rs             - SyncEngine: preview() and run()
    walker.rs          - directory walk, FileEntry, ExcludeSet
    fingerprint.rs     - hash_file (full or partial SHA-256)
    matcher.rs         - match_trees → MatchOutput
    planner.rs         - plan() → SyncPlan + SyncOp
    executor.rs        - execute() - runs a SyncPlan, emits ProgressEvents
    tests.rs           - in-crate tests for private items (tempdir-based, tokio)
  gui/
    mod.rs             - GUI entry point
    server.rs          - axum router, session-token and same-origin middleware, graceful shutdown
    handlers.rs        - HTTP handler functions
    state.rs           - AppState (shared config, progress, plan, control channels)
    ws.rs              - WebSocket handler (streams ProgressEvents to browser)
    assets.rs          - rust-embed static file serving
```
