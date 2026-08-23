<script lang="ts">
  import ContextMenu from './ContextMenu.svelte';
  import type { DisplayRow } from '../lib/treeUtils';
  import { fmtCount, formatBytes, collapsedDirs } from '../lib/store';

  let {
    rows = [],
    side = 'src',
    title = '',
    dirSizes = {},
    headerStats = null,
    scanning = false,
    scanCount = null,
    scanDetail = null,
    syncScrollTop = null,
    focusedIndex = -1,
    panelActive = false,
    containerHeight = $bindable(0),
    menuDisabled = false,
    onskip,
    onexclude,
    onscrolled,
    onselect,
  }: {
    rows?: (DisplayRow | null)[];
    side?: 'src' | 'dst';
    title?: string;
    dirSizes?: Record<string, number>;
    headerStats?: { ops: number; bytes: number } | null;
    scanning?: boolean;
    scanCount?: number | null;
    scanDetail?: string | null;
    syncScrollTop?: number | null;
    focusedIndex?: number;
    panelActive?: boolean;
    containerHeight?: number;
    menuDisabled?: boolean;
    onskip: (detail: { path: string }) => void;
    onexclude: (detail: { path: string }) => void;
    onscrolled: (detail: { scrollTop: number }) => void;
    onselect: (detail: { index: number }) => void;
  } = $props();

  // Virtual scrolling: only render rows in the visible viewport plus a small
  // buffer. With fixed 22 px row height this keeps the DOM at ~50 nodes even
  // for a 100 k-file plan, making collapse/skip interactions instant.
  const ROW_HEIGHT = 22; // must match .tree-dir/.tree-row/.tree-gap height in CSS
  const OVERSCAN = 8;   // extra rows rendered above and below the viewport

  let bodyEl: HTMLElement;
  let scrollTop = $state(0);
  let isSyncingScroll = false;

  const firstVisible = $derived(Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN));
  const lastVisible  = $derived(Math.min(rows.length, Math.ceil((scrollTop + containerHeight) / ROW_HEIGHT) + OVERSCAN));
  const visibleRows  = $derived(rows.slice(firstVisible, lastVisible));
  const topPad       = $derived(firstVisible * ROW_HEIGHT);
  const bottomPad    = $derived(Math.max(0, rows.length - lastVisible) * ROW_HEIGHT);

  $effect(() => {
    if (bodyEl && syncScrollTop !== null) {
      isSyncingScroll = true;
      bodyEl.scrollTop = syncScrollTop;
      scrollTop = syncScrollTop;
      requestAnimationFrame(() => { isSyncingScroll = false; });
    }
  });

  // Scroll the focused row into view only when focusedIndex actually changes
  // (not on every mouse scroll). The prevFocusedForScroll guard prevents
  // scrollTop from being a reactive trigger that fights the user's scrolling.
  let prevFocusedForScroll = $state(-1);
  $effect(() => {
    if (bodyEl && focusedIndex >= 0 && focusedIndex !== prevFocusedForScroll) {
      prevFocusedForScroll = focusedIndex;
      const rowTop = focusedIndex * ROW_HEIGHT;
      const rowBot = rowTop + ROW_HEIGHT;
      if (rowTop < scrollTop) {
        bodyEl.scrollTop = rowTop;
        scrollTop = rowTop;
      } else if (rowBot > scrollTop + containerHeight) {
        const newTop = rowBot - containerHeight;
        bodyEl.scrollTop = newTop;
        scrollTop = newTop;
      }
    }
  });

  function handleScroll() {
    scrollTop = bodyEl.scrollTop;
    if (!isSyncingScroll) onscrolled({ scrollTop });
  }

  let ctx: { x: number; y: number; path: string } | null = $state(null);

  function onContextMenu(e: MouseEvent, path: string) {
    e.preventDefault();
    if (menuDisabled) return;
    ctx = { x: e.clientX, y: e.clientY, path };
  }

  function closeCtx() { ctx = null; }
  function onSkip() { if (ctx) onskip({ path: ctx.path }); ctx = null; }
  function onExclude() { if (ctx) onexclude({ path: ctx.path }); ctx = null; }

  function badgeClass(badge: string): string {
    if (badge === '+' || badge === '↻') return 'badge-add';
    if (badge === '–') return 'badge-del';
    if (badge === '→') return 'badge-move';
    if (badge === '⇢') return 'badge-link';
    return 'badge-err';
  }

  function opDesc(op: import('../lib/types').OpEntry & { error?: string }): string {
    const descs: Record<string, string> = {
      copy:         'Copy to destination',
      overwrite:    'Overwrite in destination',
      move:         'Move within destination',
      delete:       'Delete from destination',
      'dir-rename': 'Rename directory',
      'case-rename': 'Rename (case only)',
      symlink:      'Create symlink in destination',
    };
    const base = descs[op.kind] ?? op.kind;
    if ((op.kind === 'move' || op.kind === 'dir-rename') && op.from_path) {
      const fromName = op.from_path.split('/').pop() ?? op.from_path;
      const toName   = op.rel_path.split('/').pop() ?? op.rel_path;
      const fromDir  = op.from_path.includes('/') ? op.from_path.slice(0, op.from_path.lastIndexOf('/')) : '';
      const toDir    = op.rel_path.includes('/')  ? op.rel_path.slice(0, op.rel_path.lastIndexOf('/'))  : '';
      const nameChanged = fromName !== toName;
      const dirChanged  = fromDir  !== toDir;
      if (nameChanged && dirChanged) return `${base} (was: ${op.from_path})`;
      if (nameChanged)               return `${base} (was: ${fromName})`;
      if (dirChanged)                return `${base} (from: ${fromDir || '/'})`;
    }
    return base;
  }

  function rowTooltip(row: DisplayRow): string {
    if (row.rowType !== 'op') return '';
    const op = row.op;
    const sizePart = op.size > 0 ? `  -  ${formatBytes(op.size)}` : '';
    const lines = [`${row.name}${sizePart}`];
    if (op.hash) lines.push(op.hash.slice(0, 8));
    lines.push(opDesc(op));
    return lines.join('\n');
  }

  function toggleDir(path: string) {
    collapsedDirs.update(s => {
      if (s.has(path)) s.delete(path); else s.add(path);
      return s;
    });
  }

  const headerCount = $derived(scanning
    ? (scanCount !== null ? `${fmtCount(scanCount)} files scanned` : 'Scanning…')
    : headerStats
      ? `${fmtCount(headerStats.ops)} ops · ${formatBytes(headerStats.bytes)}`
      : '');

</script>

<div class="tree-panel" class:panel-active={panelActive}>
  <div class="tree-header">
    <span class="tree-title">{title}</span>
    <span class="tree-count" class:scanning>{headerCount}</span>
  </div>

  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="tree-body" bind:this={bodyEl} bind:clientHeight={containerHeight} onscroll={handleScroll} role="list">
    {#if scanning}
      <div class="scan-row pulse">
        <span class="scan-spinner">⟳</span>
        {#if scanCount !== null}
          <span>Scanned {fmtCount(scanCount)} file{scanCount === 1 ? '' : 's'}</span>
        {:else if scanDetail}
          <span class="scan-path">{scanDetail}</span>
        {:else}
          <span>Scanning…</span>
        {/if}
      </div>
      {#each Array(5) as _, i}
        <div class="skeleton-row" style="opacity: {0.4 - i * 0.07}">
          <span class="skeleton-badge"></span>
          <span class="skeleton-path" style="width: {60 + (i * 7) % 30}%"></span>
          <span class="skeleton-size"></span>
        </div>
      {/each}
    {:else if rows.length === 0}
      <div class="tree-empty">Nothing to sync</div>
    {:else}
      <!-- Top spacer keeps the scrollbar thumb at the correct position. -->
      <div style="padding-top: {topPad}px">
        {#each visibleRows as row, i (firstVisible + i)}
          {#if row === null}
            <div class="tree-gap"></div>
          {:else if row.rowType === 'dir'}
            <div
              class="tree-dir"
              class:row-focused={firstVisible + i === focusedIndex}
              style="padding-left: {8 + row.depth * 16}px"
              role="button"
              tabindex="0"
              onclick={() => { onselect({ index: firstVisible + i }); toggleDir(row.path); }}
              onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') { onselect({ index: firstVisible + i }); toggleDir(row.path); } }}
              oncontextmenu={(e) => onContextMenu(e, row.path)}
            >
              <span class="dir-chevron" class:collapsed={$collapsedDirs.has(row.path)}>▾</span>
              <span class="dir-name">{row.name}/</span>
              {#if dirSizes[row.path]}
                <span class="dir-size">{formatBytes(dirSizes[row.path])}</span>
              {/if}
            </div>
          {:else}
            <div
              class="tree-row"
              class:has-error={!!row.op.error}
              class:row-focused={firstVisible + i === focusedIndex}
              style="padding-left: {8 + row.depth * 16}px"
              title={rowTooltip(row)}
              role="button"
              tabindex="0"
              onclick={() => onselect({ index: firstVisible + i })}
              onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') onselect({ index: firstVisible + i }); }}
              oncontextmenu={(e) => onContextMenu(e, row.path)}
            >
              <span class="badge {badgeClass(row.op.badge)}">{row.op.badge}</span>
              <span class="row-path">{row.name}</span>
              <span class="row-size">{row.op.size > 0 ? formatBytes(row.op.size) : ''}</span>
              {#if row.op.error}
                <span class="row-error" title={row.op.error}>⚠ {row.op.error}</span>
              {/if}
            </div>
          {/if}
        {/each}
        <!-- Bottom spacer fills the remainder of the virtual scroll height. -->
        {#if bottomPad > 0}<div style="height: {bottomPad}px"></div>{/if}
      </div>
    {/if}
  </div>
</div>

{#if ctx}
  <ContextMenu
    x={ctx.x}
    y={ctx.y}
    showSkip={side === 'src'}
    onskip={onSkip}
    onexclude={onExclude}
    onclose={closeCtx}
  />
{/if}

<style>
  .tree-panel {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--surface);
  }
  .tree-panel.panel-active {
    border-color: var(--accent-blue);
  }
  .tree-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 6px 10px;
    background: var(--header-bg);
    border-bottom: 1px solid var(--border);
    font-size: 12px;
    font-weight: 600;
    color: var(--text);
    flex-shrink: 0;
  }
  .tree-count { color: var(--text-muted); font-weight: 400; }
  .tree-count.scanning { color: var(--accent-blue); }

  .tree-body {
    flex: 1;
    overflow-y: auto;
    font-size: 12px;
    font-family: var(--font-sans);
    scrollbar-gutter: stable;
  }
  .tree-empty {
    padding: 20px;
    text-align: center;
    color: var(--text-muted);
  }

  /* Scan progress */
  .scan-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 12px;
    overflow: hidden;
    color: var(--accent-blue);
    font-size: 12px;
    font-family: var(--font-sans);
  }
  .scan-spinner {
    display: inline-block;
    animation: spin 1s linear infinite;
  }
  .scan-path {
    font-family: var(--font-mono);
    font-size: 11px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  @keyframes spin { to { transform: rotate(360deg); } }

  .pulse { animation: pulse 1.5s ease-in-out infinite; }
  @keyframes pulse {
    0%, 100% { opacity: 1; }
    50%       { opacity: 0.5; }
  }

  .skeleton-row {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 5px 10px;
    border-bottom: 1px solid var(--border-subtle);
  }
  .skeleton-badge {
    width: 16px;
    height: 10px;
    background: var(--bar-bg);
    border-radius: 2px;
    flex-shrink: 0;
  }
  .skeleton-path {
    height: 10px;
    background: var(--bar-bg);
    border-radius: 2px;
    flex: 1;
    max-width: 100%;
  }
  .skeleton-size {
    width: 44px;
    height: 10px;
    background: var(--bar-bg);
    border-radius: 2px;
    flex-shrink: 0;
  }

  /* All row types share the same fixed height for pixel-perfect scroll alignment */
  .tree-dir,
  .tree-row,
  .tree-gap {
    box-sizing: border-box;
    height: 22px;
    border-bottom: 1px solid var(--border-subtle);
  }

  /* Directory header rows */
  .tree-dir {
    display: flex;
    align-items: center;
    gap: 5px;
    padding-right: 10px;
    color: var(--text-muted);
    user-select: none;
    cursor: pointer;
  }
  .tree-dir:hover { background: var(--hover); }
  .tree-dir.row-focused,
  .tree-row.row-focused { background: var(--hover); outline: 2px solid var(--accent-blue); outline-offset: -2px; }
  .dir-chevron {
    flex-shrink: 0;
    display: inline-block;
    transition: transform 0.12s ease;
  }
  .dir-chevron.collapsed { transform: rotate(-90deg); }
  .dir-name { flex: 1; }
  .dir-size {
    margin-left: auto;
    color: var(--text-muted);
    flex-shrink: 0;
    padding-right: 2px;
  }

  /* Op rows */
  .tree-row {
    display: flex;
    align-items: center;
    gap: 6px;
    padding-right: 10px;
    cursor: default;
  }
  .tree-row:hover { background: var(--hover); }
  .tree-row.has-error { background: var(--error-bg); }

  .badge {
    flex-shrink: 0;
    width: 16px;
    text-align: center;
    font-weight: 700;
  }
  .badge-add  { color: var(--accent-green); }
  .badge-del  { color: var(--accent-red); }
  .badge-move { color: var(--accent-blue); }
  .badge-link { color: var(--accent-yellow); }
  .badge-err  { color: var(--accent-red); }

  .row-path {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text);
  }
  .row-size {
    text-align: right;
    color: var(--text-muted);
    flex-shrink: 0;
    min-width: 60px;
  }
  .row-error {
    color: var(--accent-red);
    flex-shrink: 0;
    max-width: 120px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
