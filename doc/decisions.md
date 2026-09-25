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

**Limitation:** Directories containing no files at all have an empty fingerprint and are skipped entirely, so a renamed empty directory tree becomes RmDir + MkDir rather than a Move. Separately, two directories with the same set of relative paths and file sizes but different content will be falsely identified as a rename.

**Consequence for matching:** because the pairing trusts sizes alone, files brought together *only* by a detected rename never take the mtime fast path: both sides are hashed and compared (`via_rename` in `classify_same_path`). Without that, two unrelated files with equal size and mtimes within the 3 s tolerance classified as Identical, and the old content survived under the new name for good. With it, a false pairing costs a dir-level Move plus the overwrites it needs, and the final state is correct. The cost is one hash per file on the run that detects the rename; the next run matches those files by path.

**Memory:** DST candidates are indexed by a 64-bit digest of their fingerprint, confirmed against the real fingerprint on a hit. Keying on the fingerprint itself stored every file path once per enclosing extra directory (files x depth), gigabytes for a reorganised tree of a million files.

---

## Executor phase ordering

**Decision:** the phases of `execute()`, in order:

1. **MkDir** (except those inside a renamed subtree, see 5).
2. **Hoisted cleanup** ("Phase 1.5"): the Delete/RmDir ops of any write target that DST currently holds as a *directory* (`dir_blocked_targets`, flagged by the planner from its walk).
3. **Deletes of files occupying a dir-move target.**
4. **File Moves** (topologically sorted), then **dir Moves**.
5. **MkDirs inside renamed subtrees**, which target the post-rename path.
6. **CaseRenames**, dirs before files.
7. **Symlinks**.
8. **Small copies** (parallel, or serial on HDD), then **large copies** (serial, chunked).
9. **TouchMtime**.
10. **Deletes**, then **RmDir** (deepest first).

Every phase honours pause and cancel between ops (`run_serial`).

**Why:**

- MkDir before Copies: target directories must exist before files are written into them.
- File Moves and dir Moves are independent: a detected dir rename requires an identical recursive file fingerprint, so no file move reads from or writes into a renamed subtree. File moves run first so a move can vacate a file that occupies a dir-move target (that file is a claimed move source, so it has no Delete op that step 3 could hoist).
- MkDirs inside a renamed subtree wait for the dir move: created earlier, they would materialize the rename target and make the move fail.
- Moves before Copies: a file Move's source might be at a path that a Copy would otherwise write to.
- Copies before TouchMtime: mtime correction only makes sense once the file is in its final location. A moved file whose mtime diverged gets its TouchMtime in the same plan for the same reason.
- Copies before Deletes: Deletes that would clobber a written path are suppressed by the planner (`occupied_dsts`, compared ignoring case on a case-insensitive DST).
- **A write target that DST holds as a directory.** SRC and DST are independent trees, so DST can legally have a directory where SRC has a file. `fs::rename` cannot replace a directory, and that directory's own Delete/RmDir ops would otherwise not run until step 10: so the copy failed on every first run. Step 2 hoists exactly those cleanup ops in front of every write. RmDir still refuses to remove a non-empty directory, so excluded content is never destroyed: the executor then reports the directory as blocked, naming the entries that remain, and drops the writes aimed at it instead of letting them fail with a bare "Access is denied".
- **DST files that step 1 or 2 removes are never move sources.** One sitting where SRC has a directory is removed by that directory's MkDir; one below a path where SRC has a file is removed with its directory by step 2. The matcher keeps both out of move candidacy, so they become plain orphans and the SRC side becomes a Copy. Claimed as move sources, the first was deleted before its Move ran (which then renamed the new empty directory instead) and the second blocked its own target on every run.
- Deletes before RmDir: directories must be empty before `remove_dir` is called.

---

## File Moves are topologically sorted with cycle breaking

**Decision:** File-level Move ops are sorted so that if Move A writes to a path that Move B reads from, B executes first. Cycles are broken by renaming one participant to a temporary name.

**Why:** A naive alphabetical execution of a swap (a.txt and b.txt exchanging names) would overwrite a.txt before b.txt is saved, losing content. The topological sort handles chains; the cycle-breaking handles the swap and three-way rotation cases. The temporary name pattern (`.<name>.__dirsync_swap_N__`) is chosen to be obviously synthetic and to sort out of normal filename ranges.

**Defence in depth:** today's planner cannot produce a chain or a cycle. The matcher only claims a DST file as a move source when no SRC file has that path, and a move's target is always a SRC path, so no move reads what another writes. Same-path content swaps come out as Overwrites. The sort stays because the executor must be safe for any plan it is handed, and the executor tests exercise it with hand-built plans.

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

## Case-insensitive destinations are detected at runtime

**Decision:** Whether DST resolves names ignoring case is probed at preview time (`dst_is_case_insensitive` in `matcher.rs`), not assumed from the operating system. On such a DST, case-only renames use the dedicated `SyncOp::CaseRename`, done in two steps: `rename(old -> old.__dirsync_case__)` followed by `rename(old.__dirsync_case__ -> new)`.

**Why runtime, not `#[cfg(windows)]`:** case-insensitivity belongs to the filesystem. NTFS is case-insensitive, but so is APFS in its default configuration on macOS, and so are exFAT/FAT USB drives and most SMB shares mounted on Linux: exactly the backup targets this tool is used with. With the logic compiled for Windows only, a SRC file renamed `Photo.jpg -> photo.jpg` and edited planned a Copy of `photo.jpg` plus an orphan Delete of `Photo.jpg`; on a case-insensitive volume those are the same file, so the Delete removed the fresh copy.

**Probe:** read-only. The name of an existing DST entry with its ASCII letter case flipped must resolve to the same file (same device and inode on Unix; same size and timestamps on Windows, where std exposes no stable file id). With nothing to probe there is nothing to delete either, and the platform's usual default applies (insensitive on Windows and macOS). Per-directory case sensitivity (a Windows option for WSL) and Unicode normalization differences (NFC vs NFD on macOS) are not detected.

**What changes on a case-insensitive DST:** the matcher builds a secondary `dst_by_path_lower` index next to `dst_by_path`. A SRC file with no exact-case DST counterpart is matched against it; on a hit the `MatchedEntry` carries `case_renamed_from`, and the planner emits a `CaseRename` next to any content op. The planner also suppresses orphan Deletes that equal a write target ignoring case, and repairs a new SRC dir that matches an extra DST dir ignoring case in place.

**Why two steps:** on a case-insensitive filesystem a direct `rename("PHOTO.JPG", "photo.jpg")` can be treated as a self-rename that updates nothing visible. Going through a synthetic temp name forces a fresh directory entry with the new case. If the second step fails, the first is undone: left at the staging name, the entry would be invisible to every later walk (the suffix is excluded).

**Phase ordering:** Dir `CaseRename` ops run before file `CaseRename` ops (after the moves, before the symlinks) so the directory case is corrected before any files inside it are referenced.

---

## Progress bar weights all operation types equally via a byte token

**Decision:** Every non-Copy/Overwrite op (`Move`, `Delete`, `MkDir`, `RmDir`, `Symlink`, `TouchMtime`, `CaseRename`) contributes a fixed 128 KB virtual token (`OP_TOKEN_BYTES`) to `total_bytes` in the plan, and credits that same token to `done_bytes` when it completes. The overall progress percentage is always `done_bytes / total_bytes`.

**Why:** Without weighting, `total_bytes` was zero for delete-only or move-only runs, making `overall_pct()` jump to 100 % immediately regardless of how many ops were still pending. Even in mixed runs, phases 1-4 (dirs, moves, symlinks, deletes, rmdirs) completed silently with no bar movement.

**Why a fixed token rather than actual file size for deletes:** Simplicity and predictability. Using the deleted file's real size would make a 10 GB delete dominate the bar over many small copies, which feels counterintuitive. A uniform token keeps progress movement proportional to op count, not file size, for the non-copy portion of the work.

**Why 128 KB:** Small enough that a handful of renames/deletes don't materially distort the percentage when copying gigabytes; large enough that a delete-heavy or move-heavy phase visibly advances the bar next to real copies. It started at 8 KB, which was invisible against even a modest copy phase: a thousand deletes weighed the same as one 8 MB file.

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

**Decision:** `build.rs` writes back into the working tree. On every `cargo build`:
- `README.md`: shields.io version badge

With the `gui` feature (the default) also:
- `frontend/package.json`: `"version"` field (so `__APP_VERSION__` is current for Vite)
- `frontend/package-lock.json`: top-level and `packages.""` version fields
- `frontend/src/lib/licenses_generated.ts`: auto-generated license data for the About dialog

All write calls are guarded with a content comparison and only write when the content has actually changed, so a re-build without a version bump leaves the working tree clean.

**License data is host-independent.** The file is committed, so every machine must generate the same bytes. The Rust half comes from `cargo metadata --filter-platform` for the release targets, with copyright lines read from each crate's own source; the npm half from `frontend/package-lock.json`, limited to what `frontend/src` imports (plus Vite's preload polyfill, which ships in the bundle) and skipping platform-specific packages (`os`/`cpu`). If either half cannot be collected, the existing file is kept unchanged with a warning instead of being overwritten with a partial list.

**Why not generate into `OUT_DIR`:** `licenses_generated.ts` is imported directly by Vite at build time as a TypeScript module. It must be resolvable by the TypeScript toolchain, which means it has to live in the source tree (or in a path explicitly added to `tsconfig.json`'s `paths`/`rootDirs`). Moving it to `OUT_DIR` would require non-trivial Vite plugin configuration to inject the generated path. The other three files are also consumed by tools that expect them at their canonical locations.

**Trade-offs acknowledged:**
- Builds dirty the working tree on a version bump: mitigated by the content guard and the `git status` reminder in `CLAUDE.md`.
- Two concurrent `cargo build` invocations can race on these files: acceptable for a single-developer tool.
- Read-only or sandboxed source checkouts will fail at the write step: write errors are demoted to `cargo:warning` so the build does not abort (the binary is still usable; only the version badge and license list may be stale).

**Alternative (not taken):** A CI check that *asserts* version sync rather than *performing* it, combined with a manual `cargo build` requirement before committing. This would be cleaner but requires extra CI infrastructure and more developer discipline. The current approach keeps the version fields in sync automatically at zero extra cost for the common case.

---

## Writes go through a temp file before rename

**Decision:** Both copy paths write to `<name>.__dirsync_tmp__` in the same directory, then `rename` to the final name. One function does the staging for both (`stage_and_commit`), so they cannot drift apart.

**Why:** An atomic rename ensures the destination file is never observed in a partial state. If the process is killed mid-copy, the temp file is left behind but the destination is either the old complete version or the new complete version, never a corrupted intermediate. The temp name is chosen to be local to the same directory (so the rename is always same-filesystem and therefore atomic) and obviously synthetic.

**A source that changes during the copy is not committed.** Size and mtime are read before and after; if either moved (a database or VM image rewritten in place), the op fails with "source changed during copy" and the next run retries it. Committing it would leave a torn copy stamped with SRC's mtime, which the size+mtime fast path would call Identical forever. The copy is stamped with the mtime the source had *before* the copy.

**Metadata policy:** both paths give the copy the source's mtime and permissions (`fs::copy` mirrors attributes on its own, the chunked loop did not, so behaviour used to depend on file size). A readonly DST file is replaced rather than failing: on Windows the readonly attribute is cleared before the rename and before a TouchMtime (and restored after the touch), since it otherwise blocks both with "Access is denied" on every run.

**Crashes:** a killed process is covered by the rename; a power loss is only partly covered. Large files are flushed (`sync_all`) before the rename, because a renamed-but-unflushed file can read back zero-filled while already carrying SRC's mtime. Small files skip the flush: one per file would dominate a run of thousands of them, and a small file is also re-read in full whenever its size or mtime differs.

**Cleanup:** An ordinary I/O failure removes the staging file on the way out; only a hard kill can leave one behind. Any that do survive are excluded from the walk (`BUILTIN_EXCLUDES`), because otherwise they would be hashed and could be claimed as a move source in the next run: which made leftover litter load-bearing for correctness.

---

## Endpoint validation is shared by the CLI and the GUI

**Decision:** Canonicalization, the system-critical path guard and the SRC/DST nesting checks live in `src/paths.rs` and are called by both `main.rs` and `post_preview`.

**Parents count too:** an endpoint that *contains* a system directory is refused as well: a mirror into `/usr` or `C:\Users` deletes `/usr/bin` or `C:\Users\Default` along with everything else SRC lacks. The Unix list includes the canonical macOS forms (`/private/etc`, `/private/var/db`), because the guard runs on canonical paths and `/etc` resolves to `/private/etc` there.

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

**"Add exclusion pattern" is different:** it changes what the *scan* sees, so it cannot be applied to an existing plan. The frontend marks the plan stale and Run needs a fresh preview, instead of silently executing the row the user just excluded.

---

## A run consumes its plan, even when cancelled

**Decision:** After a real run the server drops `last_plan`, whether the run finished or was cancelled. Only a dry run keeps it.

**Why:** nothing records which ops already ran. "Resuming" a cancelled run replayed the whole plan: every completed Move and Delete failed as NotFound (reported as errors against files that were in fact correct) and every copy was redone. A new preview computes exactly what is left.

**A cancel is never a failure:** a cancel that interrupts the last op (a long copy polls it mid-file) passed every between-op check; the run now ends `Cancelled` rather than `Done`, and the interrupted op is not logged as a file error.

---

## Unreadable SRC paths shield their DST counterparts

**Decision:** The walk returns the paths it could not read. An unreadable SRC or DST *root* fails the preview. For an unreadable SRC path below the root, every DST entry at or below the same relative path is left out of matching: it is neither deleted as an orphan nor claimed as a move source. The CLI prints these paths on stderr and exits 1.

**Why:** mirror semantics turn "not seen in SRC" into "delete from DST". A folder with denied list permission, an expired network-share session or a USB drive dropping out mid-walk used to read as an empty folder, so its whole mirror was planned for deletion (all of DST for an unreadable root), silently, with exit status 0. The warning went through the progress display, which prints nothing when stderr is not a terminal: exactly the cron / Task Scheduler setup. Warnings now fall back to plain stderr there.

---

## A named config file must load

**Decision:** `--config <file>` that is missing, unreadable, not UTF-8/UTF-16 text or not valid TOML is a usage error (exit 2). Only the default config location falls back to defaults, and it backs up an undecodable or unparseable file byte for byte (`config.toml.bad`) before anything can overwrite it. UTF-16 with a BOM (what Windows PowerShell 5.1's `>` writes) and UTF-8 with a BOM are decoded.

**Why:** defaults drop the job's excludes, and the DST content those excludes protect is then planned for deletion. A typo in a scheduled job's path must stop the job, not change what it deletes.

**Session-only overrides:** `-e` patterns take part in every walk but are stripped by every save, and `--port` is handed to the server directly instead of being written into the config. The GUI saves once per preview; before, a one-off flag became a permanent part of the file.

---

## Exit status is a contract

**Decision:** `0` success, nothing to do or dry run; `1` finished, but files failed or paths could not be read; `2` usage error (bad flags, missing or invalid SRC/DST, unreadable `--config`); `3` fatal error, nothing could be planned or started; `130` cancelled with Ctrl-C, whichever phase the cancel lands in.

**Why:** scripts branch on it. A cancel during the walk used to surface as "Error: cancelled" with status 1, and a missing positional or a nested pair as 1 as well, so "some files were skipped" meant four different things.

---

## The server outlives the last browser tab by a few seconds

**Decision:** The GUI server shuts down five seconds after its last WebSocket client disconnects, rather than on a `beforeunload` beacon from the page.

**Why:** `beforeunload` fires on reload exactly as it does on close, so pressing F5 shut down the backend and the reloaded page had nothing to connect to: the reconnect loop then backed off against a dead port. Waiting for a reconnect distinguishes the two cases without needing to know which one happened: a reload is back within a second, a closed tab never returns.

**Trade-off:** closing the tab during a run still ends the run, five seconds later instead of immediately. Keeping the process alive to finish a sync with no UI attached is arguably more correct, but it strands a background process the user can no longer see or cancel; the explicit Close menu item remains the immediate path.

**Who may talk to it:** only the tab opened with the per-launch session token. `Host` and `Origin` checks stop DNS rebinding and browser CSRF, but any local process can send correct headers, and loopback is shared by every user of the machine (all Linux users, every Windows RDP or fast-user-switching session). Without the token another user could point this server, which runs as you, at your files and mirror-delete them. The token travels in the URL fragment, which browsers never send in a request line or a Referer.

**How it ends:** every shutdown trigger (last client gone, SIGINT/SIGTERM, Close) goes through `AppState::request_shutdown`, which sets the cancel flag before flipping the server's shutdown watch, and `server::start` awaits the executor task after `serve` returns. The in-flight chunked copy therefore stops at its next 256 KB chunk and the executor writes `Cancelled`, instead of the runtime drop waiting for the whole file and then discarding the rest of the plan with no final status.
