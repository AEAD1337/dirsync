<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import TopBar from './components/TopBar.svelte';
  import BottomBar from './components/BottomBar.svelte';
  import TreePanel from './components/TreePanel.svelte';
  import AboutDialog from './components/dialogs/AboutDialog.svelte';
  import LicensesDialog from './components/dialogs/LicensesDialog.svelte';
  import LogModal from './components/dialogs/LogModal.svelte';
  import { api, ApiError, SyncWebSocket, isAuthError } from './lib/api';
  import { getToken } from './lib/auth';
  import {
    config, src, dst, progress, ops, opErrors, isDark, scanState, scanProgress, collapsedDirs,
    activeDirs, planMeta, pathSep, unauthorized, EMPTY_PLAN_META,
  } from './lib/store';
  import { buildDisplayRows, mergeRows, sortOps, ROW_HEIGHT, type PlanOp } from './lib/treeUtils';
  import {
    treeNav, resolveFocus, nearestRow, focusRefAt, NO_FOCUS, TREE_KEYS, type FocusRef, type Rows,
  } from './lib/treeNav';
  import { isTrapActive } from './lib/focusTrap';
  import { normalizeSep } from './lib/paths';
  import { LOG_BUFFER_CAP, appendLog, mergeLog } from './lib/log';
  import type { WsEvent, LogEntry, PlanSummary, SyncStatus } from './lib/types';

  /** Virtual progress weight of every op that is not a copy/overwrite: mirrors OP_TOKEN_BYTES in src/sync/planner.rs. */
  const OP_TOKEN_BYTES = 128 * 1024;

  let showAbout = $state(false);
  let showLicenses = $state(false);
  let showLog = $state(false);
  let previewError: string | null = $state(null);
  // Informational banner (e.g. "the plan predates a new exclusion").
  let notice: string | null = $state(null);
  let shuttingDown = $state(false);
  let prevStatus: SyncStatus = $state('idle');
  let previewing = $state(false);
  let running = $state(false);
  let driveMode: 'auto' | 'ssd' | 'hdd' = $state('auto');

  // ---------- Keyboard navigation ----------

  let activePanel: 'src' | 'dst' = $state('src');
  // Focus is held by row identity and the index re-derived from the current
  // rows: a bare index went stale on every new plan, collapse or completion.
  let srcFocus: FocusRef = $state.raw(NO_FOCUS);
  let dstFocus: FocusRef = $state.raw(NO_FOCUS);
  // Bumped on keyboard moves only: the panel scrolls the focused row into
  // view then, never merely because rows above it completed.
  let srcReveal = $state(0);
  let dstReveal = $state(0);
  let srcContainerHeight = $state(0);
  let dstContainerHeight = $state(0);

  function setFocus(side: 'src' | 'dst', ref: FocusRef, reveal = false) {
    if (side === 'src') { srcFocus = ref; if (reveal) srcReveal++; }
    else { dstFocus = ref; if (reveal) dstReveal++; }
  }

  // When the focused row leaves the display (completed, collapsed away, a new
  // plan), land on the row now at its position instead of dropping the
  // keyboard position; keep the stored index current while rows shift.
  function reanchor(side: 'src' | 'dst', rows: Rows, ref: FocusRef, index: number) {
    if (ref.id === null) return;
    if (index !== -1) {
      if (index !== ref.index) setFocus(side, { id: ref.id, index });
      return;
    }
    const near = nearestRow(rows, ref.index);
    setFocus(side, near === -1 ? NO_FOCUS : focusRefAt(rows, near));
  }

  function handleKeydown(e: KeyboardEvent) {
    // A dialog or menu owns the keyboard while open (see focusTrap).
    if (isTrapActive() || shuttingDown || $unauthorized) return;
    // Only when a tree panel (or nothing) owns the focus. Everywhere else
    // Enter/Space/arrows belong to the focused control, and Tab must run the
    // browser's own order or the action row and menu are unreachable.
    const target = e.target;
    if (target !== document.body && !(target instanceof Element && target.closest('.tree-panel'))) return;
    if (e.ctrlKey || e.altKey || e.metaKey) return;

    if (e.key === 'Tab') {
      e.preventDefault();
      activePanel = activePanel === 'src' ? 'dst' : 'src';
      return;
    }
    if (!TREE_KEYS.has(e.key)) return;
    e.preventDefault();

    const side = activePanel;
    const rows = side === 'src' ? srcRows : dstRows;
    const height = side === 'src' ? srcContainerHeight : dstContainerHeight;
    const pageRows = Math.max(1, Math.floor(height / ROW_HEIGHT) - 1);
    const index = side === 'src' ? focusedSrcIndex : focusedDstIndex;
    const action = treeNav(e.key, rows, index, pageRows, p => get(collapsedDirs).has(p));
    if (!action) return;
    if (action.kind === 'focus') {
      setFocus(side, focusRefAt(rows, action.index), true);
      return;
    }
    const path = action.path;
    collapsedDirs.update(s => {
      if (action.kind === 'collapse') s.add(path);
      else if (action.kind === 'expand') s.delete(path);
      else if (s.has(path)) s.delete(path);
      else s.add(path);
      return s;
    });
  }

  function onRowSelect(side: 'src' | 'dst', index: number) {
    activePanel = side;
    setFocus(side, focusRefAt(side === 'src' ? srcRows : dstRows, index));
  }

  // ---------- Server connection ----------

  const ws = new SyncWebSocket();

  // Scroll sync: each side holds the last scrollTop set by the OTHER panel.
  let srcSyncScrollTop: number | null = $state(null);
  let dstSyncScrollTop: number | null = $state(null);

  // Accumulate completed rel-paths; applied to the display at most 10x/s.
  // `ops` itself stays stable during a run - only the `completed` Set is
  // reassigned - so the dir-size aggregation (the expensive part of the
  // derived chain) never recomputes per flush; only the row filtering does.
  let pendingCompleted = new Set<string>();
  let completed = $state(new Set<string>());
  function flushCompleted() {
    if (pendingCompleted.size === 0) return;
    const next = new Set(completed);
    for (const p of pendingCompleted) next.add(p);
    pendingCompleted = new Set();
    completed = next;
  }

  // Per-op errors, batched the same way: a flood of failures publishes one
  // new Map per flush instead of rebuilding the op list per message.
  let pendingErrors = new Map<string, string>();
  function flushErrors() {
    if (pendingErrors.size === 0) return;
    const batch = pendingErrors;
    pendingErrors = new Map();
    opErrors.update(m => {
      const next = new Map(m);
      for (const [path, message] of batch) next.set(path, message);
      return next;
    });
  }
  function clearErrors() {
    pendingErrors = new Map();
    opErrors.set(new Map());
  }

  // Client log. GET /log is fetched once at startup; live entries that arrive
  // meanwhile wait in pendingLog and are merged behind the snapshot. After
  // that the log is client-owned: Clear empties it for good (nothing
  // refetches), and entries are appended in batches, capped like the server.
  let logEntries: LogEntry[] = $state.raw([]);
  let pendingLog: LogEntry[] = [];
  let logLoaded = false;
  let logHistoryDropped = false; // Clear pressed before the snapshot arrived

  async function loadLog() {
    let history: LogEntry[] = [];
    try {
      history = await api.getLog();
    } catch { /* keep the live entries alone */ }
    if (logHistoryDropped) history = [];
    logEntries = mergeLog(history, pendingLog);
    pendingLog = [];
    logLoaded = true;
  }

  function flushLog() {
    if (!logLoaded || pendingLog.length === 0) return;
    logEntries = appendLog(logEntries, pendingLog);
    pendingLog = [];
  }

  function clearLog() {
    logEntries = [];
    pendingLog = [];
    if (!logLoaded) logHistoryDropped = true;
  }

  // Asked before each WebSocket reconnect: a 401 cannot be fixed by retrying.
  async function probeServer(): Promise<boolean> {
    if (get(unauthorized)) return false;
    try {
      await api.system();
    } catch (err) {
      if (isAuthError(err)) return false;
    }
    return true;
  }

  onMount(async () => {
    ws.onEvent = handleWsEvent;
    ws.onNoToken = () => unauthorized.set(true);
    ws.shouldReconnect = probeServer;
    // Without a token every request would be refused: say so and stop.
    if (!getToken()) { unauthorized.set(true); return; }

    void loadLog();
    let autoPreview = false;
    try {
      const [cfg, sys] = await Promise.all([api.getConfig(), api.system()]);
      config.set(cfg);
      pathSep.set(sys.path_sep);
      if (cfg.last_src) src.set(normalizeSep(cfg.last_src, sys.path_sep));
      if (cfg.last_dst) dst.set(normalizeSep(cfg.last_dst, sys.path_sep));
      autoPreview = !!sys.auto_preview;
    } catch { /* server may not be ready yet */ }
    if (get(unauthorized)) return;

    ws.connect();

    if (autoPreview) handlePreview(true);
  });

  // Separate synchronous onMount so Svelte can invoke the cleanup function.
  // An async onMount returns a Promise, which Svelte ignores: the interval
  // would leak on unmount if registered inside the async callback above.
  onMount(() => {
    const flushTimer = setInterval(() => {
      flushCompleted();
      flushErrors();
      flushLog();
      refreshActiveDirs();
    }, 100);
    return () => clearInterval(flushTimer);
  });

  // A 401 anywhere means this page's token is wrong: stop the socket too.
  $effect(() => { if ($unauthorized) ws.disconnect(); });

  // Directories with work in flight, from two sources because neither alone
  // covers a run: completions say where the parallel small copies are but go
  // silent for the whole of a large file, and current_dir covers exactly that
  // gap. A completion keeps its directory marked for ACTIVE_HOLD_MS so the
  // 10x/s flush cannot strobe the marker as consecutive batches land in
  // different siblings. Only the immediate parent is marked: marking the
  // whole ancestor chain would keep the root lit and say nothing.
  const ACTIVE_HOLD_MS = 800;
  const dirLastSeen = new Map<string, number>();
  let copyingDir: string | null = null;

  function parentDir(relPath: string): string | null {
    const p = relPath.replace(/\\/g, '/');
    const cut = p.lastIndexOf('/');
    return cut > 0 ? p.slice(0, cut) : null;
  }

  function refreshActiveDirs() {
    const now = Date.now();
    for (const [dir, seen] of dirLastSeen) {
      if (now - seen > ACTIVE_HOLD_MS) dirLastSeen.delete(dir);
    }
    const next = new Set(dirLastSeen.keys());
    if (copyingDir) next.add(copyingDir);
    // Publish only a real change: every write re-renders both panels' rows.
    const current = get(activeDirs);
    if (next.size === current.size && [...next].every(d => current.has(d))) return;
    activeDirs.set(next);
  }

  function clearActiveDirs() {
    dirLastSeen.clear();
    copyingDir = null;
    activeDirs.set(new Set());
  }

  onDestroy(() => ws.disconnect());

  // Ops the plan holds without a display row (MkDir/RmDir). A Skip may drop
  // some of them server-side, which the client cannot count exactly.
  let hiddenOps = 0;

  // Populate the ops tree and plan metadata from a plan summary: the WS
  // plan_ready event, or GET /plan when recovering state after a reload.
  function applyPlan(plan: PlanSummary) {
    // Sort once here so buildDisplayRows can do a single linear pass. The
    // path key orders a directory directly before its children, which is
    // what keeps the rows buildDisplayRows emits ascending for mergeRows.
    ops.set(sortOps(plan.ops));
    completed = new Set();
    pendingCompleted = new Set();
    hiddenOps = Math.max(0, plan.total_ops - plan.ops.length);
    planMeta.set({ totalOps: plan.total_ops, totalBytes: plan.total_bytes });
  }

  // End-of-run bookkeeping, for 'done' and 'cancelled' alike. The server
  // drops the plan in both cases (replaying a cancelled one would redo moves
  // and deletes that already ran), so Run must wait for a fresh preview.
  // Failed rows stay, with their error. After a cancel the rows that never
  // ran stay too, so the user sees what did not happen; after 'done' any row
  // still without a completion only missed its op_completed event (a lagged
  // broadcast channel) and is cleared.
  function finishRun(cancelled: boolean) {
    flushCompleted();
    flushErrors();
    const failed = get(opErrors);
    const done = completed;
    ops.update(list => list.filter(op => failed.has(op.rel_path) || (cancelled && !done.has(op.rel_path))));
    completed = new Set();
    driveMode = 'auto';
    clearActiveDirs();
    planMeta.set(EMPTY_PLAN_META);
  }

  // A reload mid-run reconnects the WS, but plan_ready only fires at preview
  // completion: without this the panels sit at "Nothing to sync" while the
  // backend keeps executing. Ops completed before the reload keep their rows
  // until the end-of-run cleanup; that's the best the stored plan can tell us.
  let planRecoveryTried = false;

  // Status arrives twice: pushed on every transition (status_changed) and
  // sampled by the 100ms progress tick. The pushed edge is what makes the
  // 'previewing' transition observable at all: a preview that starts and is
  // cancelled inside one tick window is otherwise never seen, and the flags
  // below would stay latched. Applying it from both sources is idempotent:
  // every branch is gated on an actual change of `prevStatus`.
  function applyStatus(status: SyncStatus) {
    if ((status === 'running' || status === 'paused')
        && !planRecoveryTried && get(planMeta).totalOps === 0) {
      planRecoveryTried = true;
      api.getPlan().then(applyPlan).catch(() => {});
    }
    // Only our own preview ending clears the scan indicators. 'cancelled' is a
    // *sticky* terminal status of the last run: it can still be the reported
    // status while a fresh preview is starting up, so it must not count.
    if (get(scanState).active && prevStatus === 'previewing' && status !== 'previewing') {
      scanState.set({ active: false, src: null, dst: null });
    }
    if ((status === 'done' || status === 'cancelled') && prevStatus !== status) {
      finishRun(status === 'cancelled');
    }
    // Hand off to WS once the run is confirmed. A terminal status only counts
    // when it follows 'running': the tick sent just before POST /run was
    // processed still carries the previous run's 'done' and must not clear
    // the flag early (that briefly re-enabled Run).
    if (running && (status === 'running'
        || (prevStatus === 'running' && (status === 'done' || status === 'cancelled')))) {
      running = false;
    }
    // A cancelled preview produces no plan_ready and no preview_failed, so
    // without this the local flag stays set and Preview stays disabled.
    if (previewing && prevStatus === 'previewing' && status !== 'previewing') {
      previewing = false;
    }
    prevStatus = status;
  }

  function resetScan() {
    scanState.set({ active: false, src: null, dst: null });
    scanProgress.set({ srcPath: null, dstPath: null, globalPhase: null, globalPath: null });
  }

  function handleWsEvent(e: WsEvent) {
    switch (e.type) {
      case 'progress_update':
        progress.set(e);
        copyingDir = e.current_dir ?? null;
        applyStatus(e.status);
        break;
      case 'status_changed':
        applyStatus(e.status);
        break;
      case 'preview_failed':
        previewError = e.message;
        previewing = false;
        resetScan();
        break;
      case 'error_occurred':
        pendingErrors.set(e.path, e.message);
        break;
      case 'ops_completed': {
        const now = Date.now();
        for (const path of e.rel_paths) {
          pendingCompleted.add(path);
          const dir = parentDir(path);
          if (dir) dirLastSeen.set(dir, now);
        }
        break;
      }
      case 'shutdown':
        // Stop reconnecting first: the server is going away on purpose. Then
        // try to close the tab; browsers refuse that for tabs a script did not
        // open, so the overlay below is what the user actually sees.
        shuttingDown = true;
        ws.disconnect();
        window.close();
        break;
      case 'scan_update':
        scanState.update(s => ({ ...s, [e.side]: e.file_count }));
        if (e.side === 'src') scanProgress.update(s => ({ ...s, srcPath: 'Done.' }));
        else scanProgress.update(s => ({ ...s, dstPath: 'Done.' }));
        break;
      case 'scan_progress':
        if (e.phase === 'walking_src') {
          scanProgress.update(s => ({ ...s, srcPath: e.path, globalPhase: null }));
        } else if (e.phase === 'walking_dst') {
          scanProgress.update(s => ({ ...s, dstPath: e.path, globalPhase: null }));
        } else {
          const phase = e.phase; // narrowed to 'hashing' | 'planning' for the callback
          scanProgress.update(s => ({ ...s, globalPhase: phase, globalPath: e.path }));
        }
        break;
      case 'log_entry':
        pendingLog.push({ level: e.level, message: e.message, run: e.run });
        // Bound the backlog while GET /log is still in flight.
        if (pendingLog.length >= 2 * LOG_BUFFER_CAP) pendingLog = pendingLog.slice(-LOG_BUFFER_CAP);
        break;
      case 'drive_mode':
        driveMode = e.hdd ? 'hdd' : 'ssd';
        break;
      case 'plan_ready':
        applyPlan(e);
        clearErrors();
        resetScan();
        previewing = false;
        break;
    }
  }

  // The SRC/DST pair the current plan was previewed with; editing the inputs
  // away from it invalidates the plan client-side (the server independently
  // rejects a run whose endpoints don't match the stored plan).
  let previewedSrc = $state('');
  let previewedDst = $state('');

  async function handlePreview(auto = false) {
    if (previewing) return;
    if (!$src || !$dst) { alert('Set SRC and DST paths first.'); return; }
    previewing = true;
    driveMode = 'auto';
    ops.set([]);
    clearErrors();
    skippedPrefixes = [];
    previewError = null;
    notice = null;
    clearActiveDirs();
    planMeta.set(EMPTY_PLAN_META);
    collapsedDirs.set(new Set());
    scanState.set({ active: true, src: null, dst: null });
    previewedSrc = $src;
    previewedDst = $dst;
    try {
      // Returns 202 immediately; plan arrives via WS plan_ready event
      await api.preview($src, $dst, $config.exclude_patterns);
    } catch (err) {
      previewing = false;
      scanState.set({ active: false, src: null, dst: null });
      // The mount-time auto-preview may hit a 409 when a run or preview is
      // already active server-side (e.g. F5 during a run). That's the server
      // protecting the run: recover state silently instead of alerting.
      if (auto && err instanceof ApiError && err.status === 409) return;
      if (isAuthError(err)) return; // the banner says it
      alert(`Preview failed: ${err}`);
    }
  }

  async function handleRun() {
    if (running) return;
    running = true;
    clearErrors();
    try {
      await api.run(false, skippedPrefixes, $src, $dst);
    } catch (err) {
      running = false;
      if (!isAuthError(err)) alert(`Run failed: ${err}`);
    }
  }

  // Resolves to the pause state the server now holds, or null on failure.
  async function handlePause(): Promise<boolean | null> {
    try {
      return (await api.pause()).paused;
    } catch (err) {
      console.error('Pause failed:', err);
      return null;
    }
  }
  async function handleCancel() {
    try { await api.cancel(); } catch (err) { console.error('Cancel failed:', err); }
  }

  // Directories the user chose to skip. Filtering the `ops` store only hides
  // rows: the plan that executes lives on the server, so the prefixes are
  // sent with the run request and applied to the real plan there.
  let skippedPrefixes: string[] = $state([]);

  function handleSkip(e: { path: string }) {
    if (runActive) return;
    const prefix = e.path;
    if (!skippedPrefixes.includes(prefix)) skippedPrefixes = [...skippedPrefixes, prefix];
    let removedOps = 0;
    let removedBytes = 0;
    // Same rule as SyncPlan::without_skipped + recount on the server: deletes
    // survive a skip; a copy/overwrite weighs its size, every other op the
    // fixed progress token.
    ops.update(list => list.filter(op => {
      if (op.kind === 'delete') return true;
      const keep = !op.rel_path.startsWith(prefix + '/') && op.rel_path !== prefix;
      if (!keep) {
        removedOps++;
        removedBytes += op.kind === 'copy' || op.kind === 'overwrite' ? op.size : OP_TOKEN_BYTES;
      }
      return keep;
    }));
    // Keep the header honest: the server recounts the real plan at run time,
    // the display must not keep quoting the pre-skip totals until then. The
    // plan's row-less MkDirs under the prefix are dropped there too, but the
    // client cannot see them, so the totals are marked approximate when the
    // plan has any.
    planMeta.update(m => ({
      totalOps: Math.max(0, m.totalOps - removedOps),
      totalBytes: Math.max(0, m.totalBytes - removedBytes),
      approx: m.approx || (removedOps > 0 && hiddenOps > 0),
    }));
  }

  // Exclusion patterns shape the scan, so they only take effect at the next
  // preview (unlike Skip, which filters the stored plan at run time). The plan
  // on screen predates the new pattern: mark it stale so Run needs a fresh
  // preview rather than acting on the excluded paths.
  async function handleExclude(e: { path: string }) {
    if (runActive) return;
    const pattern = prompt('Add exclusion pattern:', e.path.split(/[\\/]/).pop() ?? '');
    if (!pattern) return;
    try {
      const exclude_patterns = [...get(config).exclude_patterns, pattern];
      // Send only the field that changed; the server merges and returns the
      // full config, including the last-used paths it owns.
      config.set(await api.putConfig({ exclude_patterns }));
    } catch (err) {
      if (!isAuthError(err)) alert(`Failed to save exclusion: ${err}`);
      return;
    }
    if (get(planMeta).totalOps > 0) {
      planMeta.set(EMPTY_PLAN_META);
      notice = `Exclusion pattern "${pattern}" saved. The plan shown predates it: run Preview again before Run.`;
    }
  }

  // Scroll sync handlers: each side drives the other via scrollTop (pixels)
  function onSrcScrolled(e: { scrollTop: number }) {
    dstSyncScrollTop = e.scrollTop;
    setTimeout(() => { dstSyncScrollTop = null; }, 0);
  }
  function onDstScrolled(e: { scrollTop: number }) {
    srcSyncScrollTop = e.scrollTop;
    setTimeout(() => { srcSyncScrollTop = null; }, 0);
  }

  // Theme: 'system' follows the OS preference, live.
  $effect(() => {
    const theme = $config.theme;
    if (theme !== 'system') {
      isDark.set(theme === 'dark');
      return;
    }
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    const apply = () => isDark.set(mq.matches);
    apply();
    mq.addEventListener('change', apply);
    return () => mq.removeEventListener('change', apply);
  });

  $effect(() => { document.body?.classList.toggle('dark', $isDark); });

  // Invalidate the previewed plan when SRC/DST are edited away from the pair
  // it was computed for: otherwise Run stays enabled and would execute the
  // stored plan against a destination the inputs no longer show. Plans
  // recovered after a reload (previewedSrc empty) rely on the server-side
  // endpoint check in POST /run instead.
  //
  // Never while busy: clearing the op list mid-run empties both panels and
  // zeroes the progress denominator while the sync keeps going against the
  // plan's own roots. TopBar locks the inputs during a run, so this is the
  // backstop for a path arriving from anywhere else.
  const planLocked = $derived(
    previewing || running || ['previewing', 'running', 'paused'].includes($progress.status)
  );
  $effect(() => {
    if (
      !planLocked &&
      $planMeta.totalOps > 0 &&
      previewedSrc &&
      ($src !== previewedSrc || $dst !== previewedDst)
    ) {
      planMeta.set(EMPTY_PLAN_META);
      ops.set([]);
    }
  });

  // SRC panel: copy/overwrite/move/symlink/touch ops. DST panel: delete/move/rename ops.
  // Symlinks and touches belong with the writes: both modify the destination.
  // The kind-filtered lists are completion-agnostic (stable during a run) so
  // the dir-size aggregations below don't recompute on every flush; the
  // per-panel row lists then drop completed ops, unless they failed.
  const srcKindOps = $derived($ops.filter(op => op.kind === 'copy' || op.kind === 'overwrite' || op.kind === 'move' || op.kind === 'symlink' || op.kind === 'touch'));
  const dstKindOps = $derived($ops.filter(op => op.kind === 'delete' || op.kind === 'move' || op.kind === 'dir-rename' || op.kind === 'case-rename'));
  const srcOps = $derived(srcKindOps.filter(op => !completed.has(op.rel_path) || $opErrors.has(op.rel_path)));
  const dstOps = $derived(dstKindOps.filter(op => !completed.has(op.rel_path) || $opErrors.has(op.rel_path)));

  // Build per-side display rows, then merge to align matching paths with gap placeholders.
  const srcDisplayRows = $derived(buildDisplayRows(srcOps, $collapsedDirs));
  const dstDisplayRows = $derived(buildDisplayRows(dstOps, $collapsedDirs));
  const mergedRows = $derived(mergeRows(srcDisplayRows, dstDisplayRows));
  const srcRows: Rows = $derived(mergedRows.map(r => r.src));
  const dstRows: Rows = $derived(mergedRows.map(r => r.dst));

  const focusedSrcIndex = $derived(resolveFocus(srcRows, srcFocus));
  const focusedDstIndex = $derived(resolveFocus(dstRows, dstFocus));
  $effect(() => reanchor('src', srcRows, srcFocus, focusedSrcIndex));
  $effect(() => reanchor('dst', dstRows, dstFocus, focusedDstIndex));

  // Dir sizes = aggregate bytes of the given ops under each ancestor dir, so a
  // parent directory shows only what will actually be transferred, not its
  // full on-disk size (which may include untouched files already in sync).
  function computeOpDirSizes(ops: PlanOp[]): Record<string, number> {
    const sizes: Record<string, number> = {};
    for (const op of ops) {
      const parts = op.rel_path.split('/').filter(Boolean);
      for (let i = 1; i < parts.length; i++) {
        const dir = parts.slice(0, i).join('/');
        sizes[dir] = (sizes[dir] ?? 0) + op.size;
      }
    }
    return sizes;
  }

  // SRC dir sizes: bytes of copy/overwrite/move/symlink ops (what's actually copied).
  const srcDirSizes = $derived(computeOpDirSizes(srcKindOps));
  // DST dir sizes: bytes of copy+overwrite ops landing under each destination dir.
  const dstDirSizes = $derived(computeOpDirSizes($ops.filter(op => op.kind === 'copy' || op.kind === 'overwrite')));

  const currentStatus = $derived($progress.status);

  // Skip/exclude only shape the next run - the executing plan was cloned
  // server-side at run start - so both are disabled while a run is active
  // instead of pretending to affect it.
  const runActive = $derived(running || currentStatus === 'running' || currentStatus === 'paused');

  // Hashing and planning are global phases shown on both panels; the walks
  // are per side.
  function scanDetail(sidePath: string | null): string | null {
    const s = $scanProgress;
    if (s.globalPhase === 'hashing') return s.globalPath ? `Fingerprinting  ${s.globalPath}` : 'Matching…';
    if (s.globalPhase === 'planning') return 'Planning…';
    return sidePath;
  }
  const srcScanDetail = $derived(scanDetail($scanProgress.srcPath));
  const dstScanDetail = $derived(scanDetail($scanProgress.dstPath));

  const headerStats = $derived($planMeta.totalOps > 0
    ? { ops: $planMeta.totalOps, bytes: $planMeta.totalBytes, approx: !!$planMeta.approx }
    : null);
</script>

<!-- No beforeunload shutdown beacon: it fired on reload too, killing the server
     on every F5. The server now shuts down a few seconds after the last
     WebSocket client goes away, which a reload beats by reconnecting. -->
<svelte:window onkeydown={handleKeydown} />

<div class="app" class:dark={$isDark}>
  <TopBar
    status={currentStatus}
    previewing={previewing}
    running={running}
    driveMode={driveMode}
    onpreview={() => handlePreview()}
    onrun={handleRun}
    onpause={handlePause}
    oncancel={handleCancel}
    onshowAbout={() => showAbout = true}
    onshowLicenses={() => showLicenses = true}
    onshowLog={() => showLog = true}
  />

  {#if $unauthorized}
    <div class="banner banner-error" role="alert">
      <span class="banner-icon">⚠</span>
      <span class="banner-msg">This page is not authorized. Open the URL printed by dirsync in the terminal.</span>
    </div>
  {/if}

  {#if previewError}
    <div class="banner banner-error" role="alert">
      <span class="banner-icon">⚠</span>
      <span class="banner-msg mono">{previewError}</span>
      <button type="button" class="banner-close" aria-label="Dismiss" onclick={() => previewError = null}>✕</button>
    </div>
  {/if}

  {#if notice}
    <div class="banner banner-info" role="status">
      <span class="banner-msg">{notice}</span>
      <button type="button" class="banner-close" aria-label="Dismiss" onclick={() => notice = null}>✕</button>
    </div>
  {/if}

  <main class="panels">
    <TreePanel
      rows={srcRows}
      side="src"
      title="Source"
      dirSizes={srcDirSizes}
      scanning={$scanState.active}
      scanCount={$scanState.src}
      scanDetail={srcScanDetail}
      syncScrollTop={srcSyncScrollTop}
      focusedIndex={focusedSrcIndex}
      revealSeq={srcReveal}
      panelActive={activePanel === 'src'}
      bind:containerHeight={srcContainerHeight}
      menuDisabled={runActive}
      onselect={(e) => onRowSelect('src', e.index)}
      onskip={handleSkip}
      onexclude={handleExclude}
      onscrolled={onSrcScrolled}
    />
    <TreePanel
      rows={dstRows}
      side="dst"
      title="Destination"
      dirSizes={dstDirSizes}
      headerStats={headerStats}
      scanning={$scanState.active}
      scanCount={$scanState.dst}
      scanDetail={dstScanDetail}
      syncScrollTop={dstSyncScrollTop}
      focusedIndex={focusedDstIndex}
      revealSeq={dstReveal}
      panelActive={activePanel === 'dst'}
      bind:containerHeight={dstContainerHeight}
      menuDisabled={runActive}
      onselect={(e) => onRowSelect('dst', e.index)}
      onskip={() => {}}
      onexclude={handleExclude}
      onscrolled={onDstScrolled}
    />
  </main>

  <BottomBar />
</div>

{#if showAbout}
  <AboutDialog onclose={() => showAbout = false} />
{/if}
{#if showLicenses}
  <LicensesDialog onclose={() => showLicenses = false} />
{/if}
{#if showLog}
  <LogModal
    entries={logEntries}
    onclose={() => showLog = false}
    onclear={clearLog}
  />
{/if}
{#if shuttingDown}
  <div class="shutdown-overlay" role="alert">
    <div class="shutdown-card">
      <strong>dirsync has stopped.</strong>
      <span>The server was shut down. You can close this tab.</span>
    </div>
  </div>
{/if}

<style>
  :global(*) { box-sizing: border-box; margin: 0; padding: 0; }

  :global(:root) {
    --font-sans: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    --font-mono: 'Cascadia Code', 'Fira Code', 'Consolas', monospace;

    /* Light theme */
    --bg: #f5f5f5;
    --surface: #ffffff;
    --header-bg: #f9f9f9;
    --bar-bg: #e5e7eb;
    --bar-bg-panel: #f9f9f9;
    --border: #e0e0e0;
    --border-subtle: #efefef;
    --text: #111;
    --text-muted: #666;
    --hover: #f0f0f0;
    --input-bg: #fff;
    --btn-bg: #f3f4f6;
    --error-bg: #fff5f5;
    --label-on-bar: rgba(0,0,0,0.75);

    --accent-blue:   #2563eb;
    --accent-green:  #16a34a;
    --accent-red:    #dc2626;
    --accent-yellow: #f59e0b;

    --progress-blue:  #60a5fa;
    --progress-green: #22c55e;
  }

  :global(.dark) {
    --bg: #1a1a1a;
    --surface: #242424;
    --header-bg: #1e1e1e;
    --bar-bg: #333;
    --bar-bg-panel: #1e1e1e;
    --border: #383838;
    --border-subtle: #2c2c2c;
    --text: #e8e8e8;
    --text-muted: #999;
    --hover: #2e2e2e;
    --input-bg: #1a1a1a;
    --btn-bg: #333;
    --error-bg: #2d1616;
    --label-on-bar: rgba(255,255,255,0.85);

    --progress-blue:  #1d4ed8;
    --progress-green: #166534;
  }

  :global(body) {
    font-family: var(--font-sans);
    background: var(--bg);
    color: var(--text);
    height: 100vh;
    overflow: hidden;
  }

  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
    background: var(--bg);
  }

  .banner {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    font-size: 12px;
    flex-shrink: 0;
  }
  .banner-error {
    background: var(--error-bg);
    border-bottom: 1px solid var(--accent-red);
    color: var(--accent-red);
  }
  .banner-info {
    background: var(--header-bg);
    border-bottom: 1px solid var(--accent-blue);
    color: var(--text);
  }
  .banner-icon { font-size: 14px; flex-shrink: 0; }
  .banner-msg { flex: 1; }
  .banner-msg.mono { font-family: var(--font-mono); word-break: break-all; }
  .banner-close {
    background: none;
    border: none;
    cursor: pointer;
    color: inherit;
    font-size: 12px;
    padding: 0 4px;
    flex-shrink: 0;
    opacity: 0.7;
  }
  .banner-close:hover { opacity: 1; }

  .shutdown-overlay {
    position: fixed;
    inset: 0;
    z-index: 300;
    background: rgba(0,0,0,0.55);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .shutdown-card {
    background: var(--surface);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 24px 32px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 13px;
    box-shadow: 0 8px 32px rgba(0,0,0,0.3);
  }

  .panels {
    flex: 1;
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
    padding: 8px;
    overflow: hidden;
    min-height: 0;
  }
</style>
