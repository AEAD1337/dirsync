<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import TopBar from './components/TopBar.svelte';
  import BottomBar from './components/BottomBar.svelte';
  import TreePanel from './components/TreePanel.svelte';
  import AboutDialog from './components/dialogs/AboutDialog.svelte';
  import LicensesDialog from './components/dialogs/LicensesDialog.svelte';
  import { api, ApiError, SyncWebSocket } from './lib/api';
  import { config, src, dst, progress, ops, errors, isDark, scanState, scanProgress, collapsedDirs, planMeta, pathSep } from './lib/store';
  import { buildDisplayRows, mergeRows, pathKey } from './lib/treeUtils';
  import { get } from 'svelte/store';
  import type { WsEvent, OpEntry, LogEntry, PlanSummary } from './lib/types';
  import LogModal from './components/dialogs/LogModal.svelte';

  function normalizeSep(p: string, sep: string): string {
    let out = sep === '\\' ? p.replace(/\//g, '\\') : p.replace(/\\/g, '/');
    if (out && !out.endsWith(sep)) out += sep;
    return out;
  }

  let showAbout = $state(false);
  let showLicenses = $state(false);
  let showLog = $state(false);
  let logEntries: LogEntry[] = $state([]);
  let previewError: string | null = $state(null);
  let shuttingDown = $state(false);
  let prevStatus = $state('idle');
  let previewing = $state(false);
  let running = $state(false);
  let driveMode: 'auto' | 'ssd' | 'hdd' = $state('auto');

  // Keyboard navigation state
  let activePanel: 'src' | 'dst' = $state('src');
  let focusedSrcIndex = $state(-1);
  let focusedDstIndex = $state(-1);
  let srcContainerHeight = $state(0);
  let dstContainerHeight = $state(0);
  const ROW_HEIGHT = 22; // must match TreePanel.svelte

  function handleKeydown(e: KeyboardEvent) {
    const target = e.target as HTMLElement;
    if (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable) return;

    const rows = activePanel === 'src' ? srcRows : dstRows;
    const focusedIndex = activePanel === 'src' ? focusedSrcIndex : focusedDstIndex;
    const setFocused = (i: number) => {
      if (activePanel === 'src') focusedSrcIndex = i;
      else focusedDstIndex = i;
    };

    switch (e.key) {
      case 'Tab': {
        e.preventDefault();
        activePanel = activePanel === 'src' ? 'dst' : 'src';
        break;
      }
      case 'ArrowUp':
      case 'ArrowDown': {
        e.preventDefault();
        const dir = e.key === 'ArrowUp' ? -1 : 1;
        let next = focusedIndex === -1
          ? (dir === 1 ? 0 : rows.length - 1)
          : focusedIndex + dir;
        while (next >= 0 && next < rows.length && rows[next] === null) next += dir;
        if (next >= 0 && next < rows.length && rows[next] !== null) setFocused(next);
        break;
      }
      case 'PageUp':
      case 'PageDown': {
        e.preventDefault();
        const containerHeight = activePanel === 'src' ? srcContainerHeight : dstContainerHeight;
        const pageRows = Math.max(1, Math.floor(containerHeight / ROW_HEIGHT) - 1);
        const dir = e.key === 'PageUp' ? -1 : 1;
        let next = focusedIndex === -1
          ? (dir === 1 ? 0 : rows.length - 1)
          : Math.max(0, Math.min(rows.length - 1, focusedIndex + dir * pageRows));
        while (next > 0 && next < rows.length - 1 && rows[next] === null) next += dir;
        if (rows[next] !== null) setFocused(next);
        break;
      }
      case 'Home': {
        e.preventDefault();
        let first = 0;
        while (first < rows.length && rows[first] === null) first++;
        if (first < rows.length) setFocused(first);
        break;
      }
      case 'End': {
        e.preventDefault();
        let last = rows.length - 1;
        while (last > 0 && rows[last] === null) last--;
        if (last >= 0 && rows[last] !== null) setFocused(last);
        break;
      }
      case 'ArrowLeft':
      case 'ArrowRight':
      case ' ':
      case 'Enter': {
        e.preventDefault();
        const row = rows[focusedIndex];
        if (!row) break;
        if (e.key === 'ArrowLeft') {
          const isExpanded = row.rowType === 'dir' && !get(collapsedDirs).has(row.path);
          if (isExpanded) {
            // Expanded dir: collapse it, stay focused here.
            collapsedDirs.update(s => { s.add(row.path); return s; });
          } else {
            // Collapsed dir or file: navigate to parent.
            for (let i = focusedIndex - 1; i >= 0; i--) {
              const r = rows[i];
              if (r && r.rowType === 'dir' && row.path.startsWith(r.path + '/')) {
                setFocused(i);
                break;
              }
            }
          }
        } else if (row.rowType === 'dir') {
          const path = row.path;
          collapsedDirs.update(s => {
            if (e.key === 'ArrowRight') s.delete(path);
            else if (s.has(path)) s.delete(path); else s.add(path); // Space or Enter
            return s;
          });
        }
        break;
      }
    }
  }
  const ws = new SyncWebSocket();

  // Scroll sync: each side holds the last scrollTop set by the OTHER panel.
  let srcSyncScrollTop: number | null = $state(null);
  let dstSyncScrollTop: number | null = $state(null);

  // Accumulate completed rel-paths; applied to the display at most 10×/s.
  // `ops` itself stays stable during a run - only the `completed` Set is
  // reassigned - so the dir-size aggregation (the expensive part of the
  // derived chain) never recomputes per flush; only the row filtering does.
  let pendingCompleted = $state(new Set<string>());
  let completed = $state(new Set<string>());
  function flushCompleted() {
    if (pendingCompleted.size === 0) return;
    const next = new Set(completed);
    for (const p of pendingCompleted) next.add(p);
    pendingCompleted = new Set();
    completed = next;
  }

  onMount(async () => {
    let autoPreview = false;
    try {
      const [cfg, sys] = await Promise.all([api.getConfig(), api.system()]);
      config.set(cfg);
      pathSep.set(sys.path_sep);
      if (cfg.last_src) src.set(normalizeSep(cfg.last_src, sys.path_sep));
      if (cfg.last_dst) dst.set(normalizeSep(cfg.last_dst, sys.path_sep));
      isDark.set(cfg.theme === 'dark');
      autoPreview = !!sys.auto_preview;
    } catch { /* server may not be ready yet */ }

    ws.onEvent = handleWsEvent;
    ws.connect();

    if (autoPreview) handlePreview(true);
  });

  // Separate synchronous onMount so Svelte can invoke the cleanup function.
  // An async onMount returns a Promise, which Svelte ignores: the interval
  // would leak on unmount if registered inside the async callback above.
  onMount(() => {
    const completedTimer = setInterval(flushCompleted, 100);
    return () => clearInterval(completedTimer);
  });

  onDestroy(() => ws.disconnect());

  // Populate the ops tree and plan metadata from a plan summary: the WS
  // plan_ready event, or GET /plan when recovering state after a reload.
  function applyPlan(plan: PlanSummary) {
    // Sort once here so buildDisplayRows can do a single linear pass.
    // pathKey orders a directory directly before its children, which is what
    // keeps the rows buildDisplayRows emits ascending for mergeRows.
    const sorted = plan.ops.slice().sort((a, b) => {
      const ka = pathKey(a.rel_path), kb = pathKey(b.rel_path);
      return ka < kb ? -1 : ka > kb ? 1 : 0;
    });
    ops.set(sorted);
    completed = new Set();
    pendingCompleted = new Set();
    planMeta.set({ totalOps: plan.total_ops, totalBytes: plan.total_bytes, srcDirSizes: plan.src_dir_sizes });
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
  function applyStatus(status: import('./lib/types').SyncStatus) {
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
    // When a run finishes, clear any ops that didn't receive an op_completed
    // event (can happen if the broadcast channel lagged under heavy load).
    if (status === 'done' && prevStatus !== 'done') {
      flushCompleted();
      ops.update(list => list.filter(op => op.error));
      completed = new Set();
      driveMode = 'auto';
    }
    if (status === 'cancelled' && prevStatus !== 'cancelled') {
      driveMode = 'auto';
    }
    // Hand off to WS once the run is confirmed (or already finished/cancelled).
    if (running && (status === 'running' || status === 'done' || status === 'cancelled')) {
      running = false;
    }
    // A cancelled preview produces no plan_ready and no error_occurred, so
    // without this the local flag stays set and Preview stays disabled.
    if (previewing && prevStatus === 'previewing' && status !== 'previewing') {
      previewing = false;
    }
    prevStatus = status;
  }

  function handleWsEvent(e: WsEvent) {
    if (e.type === 'progress_update') {
      progress.set(e);
      applyStatus(e.status);
    } else if (e.type === 'status_changed') {
      applyStatus(e.status);
    } else if (e.type === 'error_occurred') {
      if (e.path === 'preview') {
        previewError = e.message;
        previewing = false;
        scanState.set({ active: false, src: null, dst: null });
        scanProgress.set({ srcPath: null, dstPath: null, globalPhase: null, globalPath: null });
      } else {
        errors.update(list => [...list, { path: e.path, message: e.message }]);
        ops.update(list =>
          list.map(op => op.rel_path === e.path ? { ...op, error: e.message } : op)
        );
      }
    } else if (e.type === 'ops_completed') {
      for (const path of e.rel_paths) pendingCompleted.add(path);
    } else if (e.type === 'shutdown') {
      shuttingDown = true;
      window.close();
    } else if (e.type === 'scan_update') {
      scanState.update(s => ({
        ...s,
        [e.side]: e.file_count,
      }));
      if (e.side === 'src') {
        scanProgress.update(s => ({ ...s, srcPath: 'Done.' }));
      } else if (e.side === 'dst') {
        scanProgress.update(s => ({ ...s, dstPath: 'Done.' }));
      }
    } else if (e.type === 'scan_progress') {
      if (e.phase === 'walking_src') {
        scanProgress.update(s => ({ ...s, srcPath: e.path, globalPhase: null }));
      } else if (e.phase === 'walking_dst') {
        scanProgress.update(s => ({ ...s, dstPath: e.path, globalPhase: null }));
      } else if (e.phase === 'hashing' || e.phase === 'planning') {
        const phase = e.phase; // narrowed to 'hashing' | 'planning' for the callback
        scanProgress.update(s => ({ ...s, globalPhase: phase, globalPath: e.path }));
      }
    } else if (e.type === 'log_entry') {
      logEntries = [...logEntries, { level: e.level, message: e.message, run: e.run }];
    } else if (e.type === 'drive_mode') {
      driveMode = e.hdd ? 'hdd' : 'ssd';
    } else if (e.type === 'plan_ready') {
      applyPlan(e);
      errors.set([]);
      scanState.set({ active: false, src: null, dst: null });
      scanProgress.set({ srcPath: null, dstPath: null, globalPhase: null, globalPath: null });
      previewing = false;
    }
  }

  async function openLog() {
    showLog = true;
    if (logEntries.length === 0) {
      try {
        logEntries = await api.getLog();
      } catch { /* server may not be ready */ }
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
    errors.set([]);
    skippedPrefixes = [];
    previewError = null;
    planMeta.set({ totalOps: 0, totalBytes: 0, srcDirSizes: {} });
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
      alert(`Preview failed: ${err}`);
    }
  }

  async function handleRun() {
    if (running) return;
    running = true;
    errors.set([]);
    try {
      await api.run(false, skippedPrefixes, $src, $dst);
    } catch (err) {
      running = false;
      alert(`Run failed: ${err}`);
    }
  }

  async function handlePause() {
    try { await api.pause(); } catch (err) { console.error('Pause failed:', err); }
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
    ops.update(list => list.filter(op => {
      if (op.kind === 'delete') return true;
      return !op.rel_path.startsWith(prefix + '/') && op.rel_path !== prefix;
    }));
  }

  async function handleExclude(e: { path: string }) {
    if (runActive) return;
    const pattern = prompt('Add exclusion pattern:', e.path.split(/[\\/]/).pop() ?? '');
    if (!pattern) return;
    try {
      const updated = { ...get(config), exclude_patterns: [...get(config).exclude_patterns, pattern] };
      await api.putConfig(updated);
      config.set(updated);
    } catch (err) {
      alert(`Failed to save exclusion: ${err}`);
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

  $effect(() => { document.body?.classList.toggle('dark', $isDark); });

  // Invalidate the previewed plan when SRC/DST are edited away from the pair
  // it was computed for: otherwise Run stays enabled and would execute the
  // stored plan against a destination the inputs no longer show. Plans
  // recovered after a reload (previewedSrc empty) rely on the server-side
  // endpoint check in POST /run instead.
  $effect(() => {
    if (
      $planMeta.totalOps > 0 &&
      previewedSrc &&
      ($src !== previewedSrc || $dst !== previewedDst)
    ) {
      planMeta.set({ totalOps: 0, totalBytes: 0, srcDirSizes: {} });
      ops.set([]);
    }
  });

  // SRC panel: copy/overwrite/move/symlink ops. DST panel: delete/move/rename ops.
  // Symlinks belong with the writes: they create an entry in the destination.
  // The kind-filtered lists are completion-agnostic (stable during a run) so
  // the dir-size aggregations below don't recompute on every flush; the
  // per-panel row lists then drop completed ops.
  const srcKindOps = $derived($ops.filter(op => op.kind === 'copy' || op.kind === 'overwrite' || op.kind === 'move' || op.kind === 'symlink'));
  const dstKindOps = $derived($ops.filter(op => op.kind === 'delete' || op.kind === 'move' || op.kind === 'dir-rename' || op.kind === 'case-rename'));
  const srcOps = $derived(srcKindOps.filter(op => op.error || !completed.has(op.rel_path)));
  const dstOps = $derived(dstKindOps.filter(op => op.error || !completed.has(op.rel_path)));

  // Build per-side display rows, then merge to align matching paths with gap placeholders.
  const srcDisplayRows = $derived(buildDisplayRows(srcOps, $collapsedDirs));
  const dstDisplayRows = $derived(buildDisplayRows(dstOps, $collapsedDirs));
  const mergedRows = $derived(mergeRows(srcDisplayRows, dstDisplayRows));
  const srcRows = $derived(mergedRows.map(r => r.src));
  const dstRows = $derived(mergedRows.map(r => r.dst));

  // Dir sizes = aggregate bytes of the given ops under each ancestor dir, so a
  // parent directory shows only what will actually be transferred, not its
  // full on-disk size (which may include untouched files already in sync).
  function computeOpDirSizes(ops: (OpEntry & { error?: string })[]): Record<string, number> {
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

  const srcScanDetail = $derived($scanProgress.globalPhase === 'hashing'
    ? ($scanProgress.globalPath ? `Fingerprinting  ${$scanProgress.globalPath}` : 'Matching…')
    : $scanProgress.globalPhase === 'planning' ? 'Planning…'
    : $scanProgress.srcPath ?? null);

  const dstScanDetail = $derived($scanProgress.globalPhase === 'hashing'
    ? ($scanProgress.globalPath ? `Fingerprinting  ${$scanProgress.globalPath}` : 'Matching…')
    : $scanProgress.globalPhase === 'planning' ? 'Planning…'
    : $scanProgress.dstPath ?? null);
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
    onshowLog={openLog}
  />

  {#if previewError}
    <div class="preview-error">
      <span class="preview-error-icon">⚠</span>
      <span class="preview-error-msg">{previewError}</span>
      <button class="preview-error-close" onclick={() => previewError = null}>✕</button>
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
      panelActive={activePanel === 'src'}
      bind:containerHeight={srcContainerHeight}
      menuDisabled={runActive}
      onselect={(e) => { activePanel = 'src'; focusedSrcIndex = e.index; }}
      onskip={handleSkip}
      onexclude={handleExclude}
      onscrolled={onSrcScrolled}
    />
    <TreePanel
      rows={dstRows}
      side="dst"
      title="Destination"
      dirSizes={dstDirSizes}
      headerStats={$planMeta.totalOps > 0 ? { ops: $planMeta.totalOps, bytes: $planMeta.totalBytes } : null}
      scanning={$scanState.active}
      scanCount={$scanState.dst}
      scanDetail={dstScanDetail}
      syncScrollTop={dstSyncScrollTop}
      focusedIndex={focusedDstIndex}
      panelActive={activePanel === 'dst'}
      bind:containerHeight={dstContainerHeight}
      menuDisabled={runActive}
      onselect={(e) => { activePanel = 'dst'; focusedDstIndex = e.index; }}
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
    onclear={() => { logEntries = []; }}
  />
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

  .preview-error {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    background: var(--error-bg);
    border-bottom: 1px solid var(--accent-red);
    font-size: 12px;
    color: var(--accent-red);
    flex-shrink: 0;
  }
  .preview-error-icon { font-size: 14px; flex-shrink: 0; }
  .preview-error-msg { flex: 1; font-family: var(--font-mono); word-break: break-all; }
  .preview-error-close {
    background: none;
    border: none;
    cursor: pointer;
    color: var(--accent-red);
    font-size: 12px;
    padding: 0 4px;
    flex-shrink: 0;
    opacity: 0.7;
  }
  .preview-error-close:hover { opacity: 1; }

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
