# Architecture

dirsync is a one-way directory mirror. All sync logic lives in `src/sync/`; the GUI and CLI are thin shells that invoke the same engine. Both shells validate their SRC/DST pair through `src/paths.rs` before the engine sees it: the guards belong to the operation, not to one front end.

## Pipeline overview

```
Walk (parallel)
  └─ src/sync/walker.rs
        │  Vec<FileEntry> (rel_path, abs_path, size, mtime, is_dir)
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

**Exclusions** are applied per path component. A pattern matching any single component anywhere in the relative path excludes the entire subtree. Built-in exclusions (`System Volume Information`, `$Recycle.Bin`, etc.) are prepended before user patterns.

---

## Stage 2: Match (`matcher.rs`)

Produces a `MatchResult` for every SRC file, plus an orphan list for DST files with no SRC counterpart.

### Phase 0: Renamed directory detection

A directory is considered renamed when it appears in SRC under a new path but its contents (modelled as `sorted[(path_within_dir, size)]`) match a DST directory's contents exactly. This fingerprint is computed purely from walk metadata: no I/O, no hashing.

Detection processes SRC dirs shallowest-first so parent renames are claimed before child dirs. Once a dir pair is matched, its files are excluded from file-level move detection because the dir-level `Move` op handles them wholesale.

A `rename_index` maps each known `dst_rel` to its rename record, enabling O(path-depth) effective-path lookups for files inside renamed dirs.

### Phase 1: Classify without I/O

Each SRC file is looked up in `dst_by_path` (keyed by effective DST path). Three outcomes drive what gets added to the `needs_hash` set:

| Situation | What gets hashed |
|---|---|
| Same path, same size, mtimes within 3 s | Nothing - fast-path Identical |
| Same path, same size, mtimes diverge | Both SRC and DST files |
| Same path, different size | SRC only (will be Overwrite; hash stored for GUI display) |
| No same-path DST file; **Windows only:** case-insensitive path match exists, same size, mtimes diverge | Both SRC and DST files (case-rename with mtime drift) |
| No same-path DST file; **Windows only:** case-insensitive path match exists, different size | SRC only |
| No same-path DST file, size > 0, same-size DST candidates exist | SRC + all same-size DST candidates |
| No same-path DST file, size > 0, no same-size DST candidates | Nothing |
| No same-path DST file, size == 0 | Nothing - zero-byte files are excluded from move detection |

On Windows a secondary `dst_by_path_lower` index (keyed by lowercased effective path) is built alongside `dst_by_path`. When a SRC file has no exact-case DST counterpart but the lowercased lookup finds one, the Windows branch handles hashing and skips the move-detection path.

### Phase 2: Hash

`needs_hash` is split by endpoint and the two sides are hashed concurrently via `rayon::join`. Each side then runs at its own drive's pace: strictly serial for a spinning endpoint (one seek stream), `par_iter` across all cores otherwise. So two HDDs still hash simultaneously, and a mixed pair lets the SSD side use every core while the HDD side stays orderly. `fingerprint::hash_file` (see below) is called for each file.

### Phase 3: Finalize matches

A pre-pass marks every DST path that has a same-path SRC counterpart as reserved, preventing those files from being claimed as move sources by other SRC files processed earlier in the loop. On Windows the pre-pass also reserves DST paths that differ only in case, so case-mismatched files are not double-claimed as move sources.

Match results:

| `MatchResult` | Meaning |
|---|---|
| `Identical` | Same path, same size, mtimes within 3 s tolerance - no action needed |
| `IdenticalMtimeDiverged` | Same path, same size, hashes match, but mtime difference exceeds tolerance - touch DST mtime |
| `SamePathDifferentContent` | Same path, different size or different hash - overwrite |
| `MovedFrom(old_path)` | No same-path DST file, but a same-hash same-size DST file exists elsewhere |
| `NewInSrc` | No match anywhere in DST |

`MatchedEntry` carries an optional `case_renamed_from: Option<PathBuf>` field. On Windows, when a SRC file matches a DST file via the case-insensitive index (and there is no exact-case match), `case_renamed_from` is set to the DST file's current rel-path. The planner uses this to emit a `CaseRename` op instead of a `Copy`.

---

## Stage 3: Plan (`planner.rs`)

Translates `MatchOutput` into a concrete, ordered `Vec<SyncOp>`:

```rust
pub enum SyncOp {
    MkDir      { path }
    Move       { from, to, is_dir }
    #[cfg(windows)]
    CaseRename { from, to, is_dir }   // case-only rename on NTFS (two-step via temp)
    Copy       { src, dst, size, hash }
    Overwrite  { src, dst, size, hash }
    Symlink    { target, dst }
    TouchMtime { src, dst }
    Delete     { path, size }
    RmDir      { path }
}
```

The ordering within the op list mirrors execution phase order (see Stage 4). `RmDir` ops are sorted deepest-first so each `remove_dir` call finds an already-empty directory.

Counters (`copy_count`, `overwrite_count`, `move_count`, `delete_count`, `identical_count`, `touch_count`, `symlink_count`) and `total_bytes` are accumulated here and exposed in `SyncPlan` for the GUI and CLI summary.

`total_bytes` is the primary progress-bar denominator. It includes:
- actual file bytes for every `Copy` and `Overwrite` op
- a fixed 8 KB virtual token (`OP_TOKEN_BYTES`) for every other op (`Move`, `Delete`, `MkDir`, `RmDir`, `Symlink`, `TouchMtime`, `CaseRename`)

This ensures all operation types advance the overall progress bar, not just file copies.

---

## Stage 4: Execute (`executor.rs`)

Ops are partitioned by type and executed in fixed phase order:

1. **MkDir**: serial; must precede all writes. `create_dir_all` is used, so missing parent dirs are never a problem. If a plain file occupies the target path (type conflict - SRC has a dir, DST has a file), the file is removed first.
2. **Move (dirs)**: serial, before file moves. A pre-pass deletes any DST files whose path would collide with a dir-move target (ENOTDIR/ERROR_ALREADY_EXISTS protection).
3. **Move (files)**: topologically sorted. If move A would overwrite move B's source, B runs first. Cycles (e.g. a ↔ b swap) are broken by renaming one participant to a temp name (`.<name>.__dirsync_swap_N__`) before continuing.
4. **CaseRename** *(Windows only)*: serial; dirs first, then files. Each rename is a two-step via a temporary name (`<name>.__dirsync_case__`) to force NTFS to update the stored directory-entry case (a direct `rename(old_case → new_case)` is a no-op on NTFS for case-only changes).
5. **Symlink**: serial, near-instant. Any existing entry at the destination (regular file or symlink) is removed first, then `symlink()` / `CreateSymbolicLink()` creates the new link.
6. **Copy / Overwrite (small, ≤ 1 MB)**: up to 8 concurrent workers via Tokio + Semaphore, or strictly serial when the drive profile reports either endpoint as spinning media (`serial_copies()`), since a copy reads SRC and writes DST in the same operation. Uses `std::fs::copy`, which resolves to `copy_file_range(2)` on Linux and `CopyFileEx` on Windows. Written to a temp file first (`.<name>.__dirsync_tmp__`), then atomically renamed, so DST is never left in a partial state. SRC mtime is preserved on DST after the rename.
7. **Copy / Overwrite (large, > 1 MB)**: serial, chunked (256 KB buffer), with per-chunk progress events (~10/s). Same temp-then-rename pattern. SRC mtime is preserved.
8. **TouchMtime**: serial; cheap metadata-only writes (`set_file_mtime`). Corrects DST files whose content is identical to SRC but whose mtime has drifted outside the 3 s tolerance.
9. **Delete**: serial file removals.
10. **RmDir**: serial directory removals, deepest-first.

A safety gate at the start of `execute()` verifies every write target is inside `dst_root` before any op runs.

Every non-Copy/Overwrite op that completes successfully calls `progress.record_bytes(OP_TOKEN_BYTES)` to credit its 8 KB token, keeping `done_bytes / total_bytes` consistent as the single progress metric throughout the run.

---

## Hashing (`fingerprint.rs`)

`hash_file` computes SHA-256. For files ≤ 1 MB the full content is hashed. For larger files only the first 512 KB and last 512 KB are read and hashed together. This makes the hash a fast probabilistic identity check rather than a cryptographic guarantee: good enough for sync decisions, and orders of magnitude faster for large media files.

The 512 KB chunk size means two large files with identical head and tail but different middles will collide. This is an accepted trade-off: the probability is negligible for real-world sync scenarios, and the alternative (full hashing) would dominate runtime for large video/image libraries.

---

## Module map

```
src/
  lib.rs               - library root; re-exports SyncEngine
  main.rs              - thin entrypoint; CLI arg parsing → run GUI or CLI mode
  cli.rs               - CLI argument definitions
  cli_ui.rs            - terminal progress display for CLI mode
  config.rs            - AppConfig (serde, platform config dir, defaults)
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
    tests.rs           - integration tests (tempdir-based, tokio)
  gui/
    mod.rs             - GUI entry point
    server.rs          - axum router, same-origin middleware, graceful shutdown
    handlers.rs        - HTTP handler functions
    state.rs           - AppState (shared config, progress, plan, control channels)
    ws.rs              - WebSocket handler (streams ProgressEvents to browser)
    assets.rs          - rust-embed static file serving
```
