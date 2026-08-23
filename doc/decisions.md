# Design Decisions

Non-obvious choices and why they were made. Intended to answer "why is it done this way?" for future maintainers.

---

## Partial hashing (head + tail, 512 KB each)

**Decision:** Files larger than 1 MB are hashed by reading only the first and last 512 KB, not the full content.

**Why:** A full SHA-256 of a 50 GB video file takes several seconds of I/O per file. Sync runs over large media libraries need to stay interactive. Partial hashing reduces that to a fixed ~1 MB read regardless of file size.

**Trade-off:** Two files with identical head and tail but different middles will collide. In practice this is vanishingly unlikely for real files: it would require adversarially crafted content or a bug that corrupts only the middle of a file while leaving boundaries intact. The threshold (1 MB full, 512 KB chunks) was chosen so that small text files, configs, and source code are always fully hashed while only large media files use the partial path.

---

## MTIME_TOLERANCE of 3 seconds

**Decision:** Mtimes within 3 seconds of each other are treated as equal without hashing.

**Why:** FAT32 stores mtime with 2-second granularity. When copying between NTFS (1-second or 100ns resolution) and FAT32, or across network shares with coarser timestamps, the same file can appear with a mtime difference of up to 2 seconds. The 3-second window covers FAT32 rounding plus a 1-second margin for other filesystem quirks. Without this tolerance, every file synced to a FAT32 volume would be re-hashed on every subsequent run.

**Trade-off:** A genuine content change that also happens to shift the mtime by exactly 1 or 2 seconds would be missed by the fast path and caught only if the file size also changed, or if the mtime difference exceeds 3 seconds. This is acceptable because content changes almost always accompany a meaningful mtime update.

---

## TouchMtime as a separate op

**Decision:** When a file is hash-identical but its DST mtime is outside the 3-second tolerance, a dedicated `TouchMtime` op is emitted rather than treating the file as `Identical`.

**Why:** Without this, the DST mtime never converges to SRC. Every subsequent preview would re-hash both files (mtime outside tolerance forces hashing), confirm they are identical, and emit no op: wasting I/O on every run indefinitely. `TouchMtime` corrects the mtime in one metadata-only write, making subsequent runs hit the fast path (`Identical` via mtime alone).

**Alternative considered:** Fold the mtime fix into `Identical` by silently touching the mtime without a separate op. Rejected because it would be invisible in the plan summary and progress output, making it harder to diagnose sync behaviour.

---

## Zero-byte files excluded from move detection

**Decision:** Files with `size == 0` are never considered as move candidates, even if a same-path DST file does not exist.

**Why:** All empty files have the same SHA-256 (the hash of zero bytes). Without the exclusion, the first empty SRC file with no same-path DST counterpart would be matched against any empty DST file, potentially generating spurious `Move` ops and consuming a DST file that belongs to a different SRC path. There is no meaningful identity signal for empty files beyond their path.

---

## Directory rename detection uses size-only fingerprints

**Decision:** A directory's identity fingerprint for rename detection is `sorted[(relative_path, size)]`: no mtime, no hash.

**Why:** The goal of rename detection is to avoid re-copying an entire subtree when it has simply been moved. An I/O-free fingerprint (using only walk metadata) keeps Phase 0 cheap even for directories containing thousands of files. Mtime is excluded because it varies across filesystems and sync operations. Hash is excluded because computing it for every file in every candidate directory would be expensive and would duplicate the work done in Phase 2.

**Limitation:** Directories containing no files at all have an empty fingerprint and are skipped entirely, so a renamed empty directory tree becomes RmDir + MkDir rather than a Move. Separately, two directories with the same set of relative paths and file sizes but different content will be falsely identified as a rename. Their files will then be matched by hash in Phase 3 and resolved correctly (as overwrites), so the final state is still correct: only the intermediate representation (a dir-level Move op instead of per-file ops) is suboptimal.

---

## Executor phase ordering

**Decision:** MkDir → dir Moves → file Moves → Copies/Overwrites → TouchMtime → Deletes → RmDir.

**Why:** Each phase depends on the previous being complete.

- MkDir before Copies: target directories must exist before files are written into them.
- Dir Moves before file Moves: a dir-level rename changes the effective path of every file inside it; file-level moves must resolve against the post-rename layout.
- File Moves before Copies: a file Move's source might be at a path that a Copy would otherwise write to, causing a conflict.
- Copies before TouchMtime: mtime correction only makes sense after the file is confirmed to be in its final location.
- Copies before Deletes: a Delete might target a path that is also a Move destination; by running all Copies first the plan has already accounted for this (Deletes that would clobber a moved file are suppressed by the planner).
- **Exception: a copy target that DST holds as a directory.** SRC and DST are independent trees, so DST can legally have a directory where SRC has a file. `fs::rename` cannot replace a directory, and that directory's own Delete/RmDir ops would not run until phases 4 and 5: so the copy failed on every first run and left a staging file behind. The executor now hoists exactly those cleanup ops in front of the copy phase (`blocking_copy_deletes` / `blocking_copy_rmdirs`), mirroring the pre-deletion already done for dir-move targets. RmDir still refuses to remove a non-empty directory, so excluded content blocks the copy with a clear error rather than being destroyed.
- Deletes before RmDir: directories must be empty before `remove_dir` is called; file deletions clear the way.

---

## File Moves are topologically sorted with cycle breaking

**Decision:** File-level Move ops are sorted so that if Move A writes to a path that Move B reads from, B executes first. Cycles are broken by renaming one participant to a temporary name.

**Why:** A naive alphabetical execution of a swap (a.txt ↔ b.txt) would overwrite a.txt before b.txt is saved, losing content. The topological sort handles chains; the cycle-breaking handles the swap and three-way rotation cases. The temporary name pattern (`.<name>.__dirsync_swap_N__`) is chosen to be obviously synthetic and to sort out of normal filename ranges.

---

## Same-path files are reserved before move detection

**Decision:** Before the move-detection loop, every DST path that has a same-path SRC counterpart is pre-inserted into `dst_matched`, preventing it from being claimed as a move source.

**Why:** Without the pre-pass, a SRC file processed early in the loop could claim a DST file as its move source even though a later SRC file at the same path as that DST file would have matched it correctly. This would generate a spurious Move op and leave the later SRC file without a DST counterpart, causing an incorrect Copy on every subsequent run.

---

## Small and large file copy paths are separate

**Decision:** Files ≤ 1 MB use `std::fs::copy` (single syscall); files > 1 MB use a manual 256 KB chunk loop with progress events.

**Why:** `std::fs::copy` on Linux resolves to `copy_file_range(2)` (kernel-space copy, zero user-space buffer) and on Windows to `CopyFileEx` (OS-optimised). For small files this is significantly faster than a user-space loop. Large files need the chunk loop so the progress bar can update during the copy: a single `fs::copy` call for a 10 GB file would block for tens of seconds with no feedback. The chunk size is 256 KB, chosen as a balance between syscall overhead and per-chunk progress granularity.

The 1 MB threshold matches the partial-hashing threshold, which is a coincidence of both being natural "small vs. large" dividing lines, not a deliberate coupling.

---

## Matching phases 1 and 3 are single-threaded

**Decision:** The `needs_hash` classification loop (Phase 1) and the final matching loop (Phase 3) in `matcher.rs` run on one CPU core.

**Why not parallelised yet:** Phase 1 can be replaced with `par_iter().flat_map().collect::<HashSet<_>>()` because `dst_by_path` and `dst_by_size` are immutable and `Sync`. Phase 3 is harder: the same-path classification arm can be parallelised with a parallel `map` followed by a sequential apply, but the move-detection arm must remain sequential because it mutates the shared `dst_matched` claim set. The benefit is meaningful only at very high file counts (1M+); for typical workloads the single-threaded O(N) loop is fast enough. Implement when profiling confirms this is the bottleneck.

**How to tackle it:** Phase 1: `par_iter().flat_map(|src| { … }).collect()`. Phase 3: parallel `map` producing `(src, MatchResult)` pairs for same-path files, then a sequential pass to insert into `dst_matched` / `matched`; keep the move-detection arm sequential with its existing `dst_by_hash` O(1) lookup.

---

## Windows NTFS case-only renames use a two-step via a temp name

**Decision:** On Windows, renaming a file or directory where only the case differs (e.g. `Photos` → `photos`) is done in two steps: `rename(old → old.__dirsync_case__)` followed by `rename(old.__dirsync_case__ → new)`. This path is compiled in only under `#[cfg(windows)]` and uses the dedicated `SyncOp::CaseRename` variant.

**Why:** NTFS is case-insensitive for lookups but case-preserving for storage. A direct `rename("PHOTO.JPG", "photo.jpg")` is treated as a self-rename and is either a no-op or updates nothing visible. The two-step forces NTFS to write a fresh directory entry with the new case. The temp suffix (`.__dirsync_case__`) is chosen to be obviously synthetic and to avoid colliding with normal filenames.

**Detection:** Rust's `PathBuf` uses byte-level comparison, so `"photo.jpg" != "PHOTO.JPG"` even on Windows. The matcher builds a secondary `dst_by_path_lower` index (keyed by lowercased path string) alongside the primary `dst_by_path`. A SRC file with no exact-case DST counterpart is matched against this index; on a hit the `MatchedEntry` carries `case_renamed_from: Some(dst_rel_path)`. The planner converts this into a `CaseRename` op instead of a `Copy`.

**Phase ordering:** Dir `CaseRename` ops run before file `CaseRename` ops (Phase 2.5, after file `Move`s and before `Symlink`s) so the directory case is corrected before any files inside it are referenced.

**Why `#[cfg(windows)]` only:** macOS and Linux filesystems are generally case-sensitive; an exact-case match either succeeds or fails, and no workaround is needed. Compiling the variant unconditionally would require dead-code suppression on non-Windows targets.

---

## Progress bar weights all operation types equally via a byte token

**Decision:** Every non-Copy/Overwrite op (`Move`, `Delete`, `MkDir`, `RmDir`, `Symlink`, `TouchMtime`, `CaseRename`) contributes a fixed 8 KB virtual token to `total_bytes` in the plan, and credits that same token to `done_bytes` when it completes. The overall progress percentage is always `done_bytes / total_bytes`.

**Why:** Without weighting, `total_bytes` was zero for delete-only or move-only runs, making `overall_pct()` jump to 100 % immediately regardless of how many ops were still pending. Even in mixed runs, phases 1-4 (dirs, moves, symlinks, deletes, rmdirs) completed silently with no bar movement.

**Why a fixed token rather than actual file size for deletes:** Simplicity and predictability. Using the deleted file's real size would make a 10 GB delete dominate the bar over many small copies, which feels counterintuitive. A uniform token keeps progress movement proportional to op count, not file size, for the non-copy portion of the work.

**Why 8 KB:** Small enough that a handful of renames/deletes don't materially distort the percentage when copying gigabytes; large enough to be visible when the plan contains only metadata-class ops.

**Trade-off:** ETA and MB/s are still byte-based. For ops-only runs (no copies) these will show trivially small values (a few KB/s) rather than meaningful throughput, which is acceptable since such runs complete almost instantly.

---

## Drive type is auto-detected rather than user-selected

**Decision:** Drive type is probed per endpoint before each run and drives I/O scheduling automatically. The manual `--hdd` flag has been removed.

**Scheduling is per endpoint, not global.** SRC and DST are assumed to live on independent devices, so work that touches only one side runs at that side's own pace:

- **Walking**: always concurrent. Each walk reads exactly one endpoint, so the two never contend for the same spindle regardless of media type.
- **Fingerprinting**: one hashing stream per endpoint, run concurrently. Each stream is serial for spinning media (a single seek stream) and rayon-parallel across all cores for an SSD. Two HDDs therefore hash simultaneously; a mixed pair lets the SSD side use every core while the HDD side stays orderly.
- **Copying**: serial as soon as *either* endpoint is spinning media. Unlike the phases above, a copy reads SRC and writes DST in the same operation, so it can never be reduced to a single device; concurrent copies would cause a seek storm on whichever side is an HDD.

**Why not one global "HDD mode" flag:** collapsing both endpoints into `src_hdd || dst_hdd` serialized work that touches only the fast drive: a single HDD anywhere dragged the whole run down to serial scanning, including a fully idle SSD on the other side.

**Assumption: SRC and DST are separate physical drives.** No same-device detection is performed. Syncing between two directories on the *same* spinning disk will parallelize scanning across one spindle and run slower than a serial scan would. This is the deliberate trade: the common case is a sync between two drives, and physical-device identity queries (`IOCTL_STORAGE_GET_DEVICE_NUMBER` on Windows, block-device resolution elsewhere) are not worth the platform-specific `unsafe` surface for the same-disk case alone.

**Why:** Requiring users to know whether their drives are HDDs and pass the right flag is error-prone: they may forget it, or not know which type applies.

**Platform-specific detection:**

- **Windows**: uses `IOCTL_STORAGE_QUERY_PROPERTY` with `StorageDeviceTrimProperty` (`DeviceIoControl`). `TrimEnabled = true` → SSD; anything else (no TRIM support, failed query, UNC path without a drive letter) → HDD. This is more reliable than the seek-penalty IOCTL that sysinfo uses on Windows: USB flash drives and virtual/encrypted volumes incorrectly answer the seek-penalty query as "yes" (HDD) regardless of actual media type, whereas TRIM support is correctly reflected even through VeraCrypt (which passes TRIM through to the host device). Defaulting failures to HDD is the conservative choice: serial I/O on an SSD is safe, whereas parallel I/O on a real HDD degrades throughput.

- **Linux / macOS**: uses sysinfo, which reads `/sys/block/*/queue/rotational` on Linux and IOKit on macOS. Both are accurate for physical drives without the USB/virtual-volume caveats that affect the Windows seek-penalty path.

**Default for unresolved:** On Windows, any query failure defaults to HDD (serial hashing on that endpoint, serial copies): the conservative safe choice when storage type is unknown. On Linux/macOS, sysinfo unknowns default to SSD (parallel I/O), which is appropriate since the rotational-flag path is accurate for physical drives and unknown results are rare.

**Trade-off:** USB flash drives without TRIM support would be classified as HDD on Windows, which is suboptimal (serial I/O is safe but slower). In practice, TRIM is supported on virtually all USB flash drives made after ~2015.

---

## build.rs writes into the source tree

**Decision:** `build.rs` writes four files back into the working tree on every `cargo build`:
- `README.md`: shields.io version badge
- `frontend/package.json`: `"version"` field (so `__APP_VERSION__` is current for Vite)
- `frontend/package-lock.json`: top-level and `packages.""` version fields
- `frontend/src/lib/licenses_generated.ts`: auto-generated license data for the About dialog

All four write calls are guarded with a content comparison and only write when the content has actually changed, so a re-build without a version bump leaves the working tree clean.

**Why not generate into `OUT_DIR`:** `licenses_generated.ts` is imported directly by Vite at build time as a TypeScript module. It must be resolvable by the TypeScript toolchain, which means it has to live in the source tree (or in a path explicitly added to `tsconfig.json`'s `paths`/`rootDirs`). Moving it to `OUT_DIR` would require non-trivial Vite plugin configuration to inject the generated path. The other three files are also consumed by tools that expect them at their canonical locations.

**Trade-offs acknowledged:**
- Builds dirty the working tree on a version bump: mitigated by the content guard and the `git status` reminder in `CLAUDE.md`.
- Two concurrent `cargo build` invocations can race on these files: acceptable for a single-developer tool.
- Read-only or sandboxed source checkouts will fail at the write step: write errors are demoted to `cargo:warning` so the build does not abort (the binary is still usable; only the version badge and license list may be stale).

**Alternative (not taken):** A CI check that *asserts* version sync rather than *performing* it, combined with a manual `cargo build` requirement before committing. This would be cleaner but requires extra CI infrastructure and more developer discipline. The current approach keeps the version fields in sync automatically at zero extra cost for the common case.

---

## Writes go through a temp file before rename

**Decision:** Both copy paths write to `<name>.__dirsync_tmp__` in the same directory, then `rename` to the final name.

**Why:** An atomic rename ensures the destination file is never observed in a partial state. If the process is killed mid-copy, the temp file is left behind but the destination is either the old complete version or the new complete version, never a corrupted intermediate. The temp name is chosen to be local to the same directory (so the rename is always same-filesystem and therefore atomic) and obviously synthetic.

**Cleanup:** An ordinary I/O failure removes the staging file on the way out; only a hard kill can leave one behind. Any that do survive are excluded from the walk (`BUILTIN_EXCLUDES`), because otherwise they would be hashed and could be claimed as a move source in the next run: which made leftover litter load-bearing for correctness.

---

## Endpoint validation is shared by the CLI and the GUI

**Decision:** Canonicalization, the system-critical path guard and the SRC/DST nesting checks live in `src/paths.rs` and are called by both `main.rs` and `post_preview`.

**Why:** They originally lived in `gui/handlers.rs`, so CLI mode reached the sync engine with no validation at all: and `--yolo`, documented as "disable system-critical path checks", was parsed but never read outside the GUI branch. That left two destructive configurations reachable from a single command:

- **SRC inside DST** (`dirsync D:\Backup\photos D:\Backup`): everything else in DST is an orphan relative to the new SRC, so a one-way mirror deletes it. Correct mirror semantics, catastrophic intent mismatch.
- **DST inside SRC** (`dirsync D:\data D:\data\backup`): each run walks SRC, finds the previous run's output inside it, and copies one level deeper. Never converges; fills the disk.

**Order matters:** canonicalize *first*. `is_system_critical` does prefix matching, so `C:\Users\..\Windows` only resolves to a blocked path once `..` is gone. The nesting checks likewise compare canonical paths, or `D:\a\..\b` would not be recognised as `D:\b`.

**What the engine receives:** the paths the user typed, not the canonical forms. Validation resolves them internally, but feeding `\?\`-prefixed extended-length paths to the engine would put them in every log line and error message for no benefit.

---

## Skipping a directory filters the plan server-side

**Decision:** "Skip this directory" accumulates prefixes in the frontend and sends them with the run request; `SyncPlan::without_skipped` drops the matching write ops and recomputes counts and byte totals.

**Why:** The context-menu action used to filter only the frontend `ops` store. The plan that executes lives on the server and was never touched, so the rows vanished from the preview and the files were copied anyway. For a tool whose whole value is "see exactly what will happen before it happens", a control that silently lies about the plan is worse than no control.

**Why deletes survive a skip:** skipping a *source* directory suppresses writes into DST; it does not mean "leave DST alone". Orphan cleanup under that path still runs, matching what the frontend's own display filter has always done (it keeps delete rows).

**Why not mutate `last_plan`:** the stored plan stays the preview the user was shown. Filtering happens on a clone at run time, so re-running without the skips needs no re-preview.

---

## The server outlives the last browser tab by a few seconds

**Decision:** The GUI server shuts down five seconds after its last WebSocket client disconnects, rather than on a `beforeunload` beacon from the page.

**Why:** `beforeunload` fires on reload exactly as it does on close, so pressing F5 shut down the backend and the reloaded page had nothing to connect to: the reconnect loop then backed off against a dead port. Waiting for a reconnect distinguishes the two cases without needing to know which one happened: a reload is back within a second, a closed tab never returns.

**Trade-off:** closing the tab during a run still ends the run, five seconds later instead of immediately. Keeping the process alive to finish a sync with no UI attached is arguably more correct, but it strands a background process the user can no longer see or cancel; the explicit Close menu item remains the immediate path.
