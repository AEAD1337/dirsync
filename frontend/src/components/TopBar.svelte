<script lang="ts">
  import { api } from '../lib/api';
  import { src, dst, config, isDark, planMeta, pathSep } from '../lib/store';
  import type { BrowseEntry } from '../lib/types';

  const {
    status = 'idle',
    previewing = false,
    running = false,
    driveMode = 'auto',
    onpreview,
    onrun,
    onpause,
    oncancel,
    onshowAbout,
    onshowLicenses,
    onshowLog,
  }: {
    status?: string;
    previewing?: boolean;
    running?: boolean;
    driveMode?: 'auto' | 'ssd' | 'hdd';
    onpreview: () => void;
    onrun: () => void;
    onpause: () => void;
    oncancel: () => void;
    onshowAbout: () => void;
    onshowLicenses: () => void;
    onshowLog: () => void;
  } = $props();

  let menuOpen = $state(false);
  let pausing = $state(false);
  let browsingSide: 'src' | 'dst' | null = $state(null);
  let browseEntries: BrowseEntry[] = $state([]);
  let browsePath = $state('');

  // Path validity: null = unchecked (empty), true = valid dir, false = missing/file
  let srcExists: boolean | null = $state(null);
  let dstExists: boolean | null = $state(null);

  // Autocomplete state per side
  let srcCompletions: string[] = $state([]);
  let dstCompletions: string[] = $state([]);
  let srcDropdown = $state(false);
  let dstDropdown = $state(false);
  let srcHighlight = $state(-1);
  let dstHighlight = $state(-1);

  // Refs for the 4 path-row focusable elements (SRC input → SRC Browse → DST input → DST Browse).
  let srcInput: HTMLInputElement = $state(undefined as any);
  let srcBrowseBtn: HTMLButtonElement = $state(undefined as any);
  let dstInput: HTMLInputElement = $state(undefined as any);
  let dstBrowseBtn: HTMLButtonElement = $state(undefined as any);

  function cyclePathFocus(e: KeyboardEvent) {
    if (e.key !== 'Tab') return;
    const elements = [srcInput, srcBrowseBtn, dstInput, dstBrowseBtn];
    const idx = elements.indexOf(e.target as HTMLInputElement);
    if (idx === -1) return;
    e.preventDefault();
    e.stopPropagation(); // prevent App.svelte's window handler from switching panels
    elements[(idx + 1) % elements.length].focus();
  }

  let statTimers: Record<string, ReturnType<typeof setTimeout>> = {};
  let completeTimers: Record<string, ReturnType<typeof setTimeout>> = {};

  // One in-flight stat per side: a path on a stalled network share or a
  // sleeping drive can take seconds to answer, and the 500ms poller below
  // must not stack requests behind it.
  let statInFlight: Record<string, boolean> = {};

  async function runStatCheck(path: string, side: 'src' | 'dst') {
    if (statInFlight[side]) return;
    statInFlight[side] = true;
    let ok = false;
    try {
      const s = await api.stat(path);
      ok = s.exists && s.is_dir;
    } catch {
      ok = false;
    } finally {
      statInFlight[side] = false;
    }
    // Drop the result if the user edited the path while the stat was running.
    if (path !== (side === 'src' ? $src : $dst)) return;
    if (side === 'src') srcExists = ok;
    else dstExists = ok;
  }

  function scheduleStatCheck(path: string, side: 'src' | 'dst') {
    clearTimeout(statTimers[side]);
    if (!path.trim()) {
      if (side === 'src') srcExists = null;
      else dstExists = null;
      return;
    }
    statTimers[side] = setTimeout(() => runStatCheck(path, side), 350);
  }

  // Availability changes without any input event, in both directions: the
  // user mounts the container, plugs the drive back in or the share
  // reconnects, and equally unmounts, unplugs or loses it again. Re-stat both
  // sides so the GUI tracks that on its own instead of waiting for an edit.
  $effect(() => {
    const timer = setInterval(() => {
      if (srcExists !== null && $src.trim()) runStatCheck($src, 'src');
      if (dstExists !== null && $dst.trim()) runStatCheck($dst, 'dst');
    }, 500);
    return () => clearInterval(timer);
  });

  function scheduleComplete(path: string, side: 'src' | 'dst') {
    clearTimeout(completeTimers[side]);
    completeTimers[side] = setTimeout(async () => {
      try {
        const r = await api.complete(path);
        if (side === 'src') {
          srcCompletions = r.completions;
          srcHighlight = -1;
          srcDropdown = r.completions.length > 0;
        } else {
          dstCompletions = r.completions;
          dstHighlight = -1;
          dstDropdown = r.completions.length > 0;
        }
      } catch {
        if (side === 'src') { srcCompletions = []; srcDropdown = false; }
        else { dstCompletions = []; dstDropdown = false; }
      }
    }, 120);
  }

  // Convert slashes to the OS-native separator, but do not append a trailing one.
  // Used while the user is actively typing.
  function convertSep(p: string): string {
    const sep = $pathSep;
    return sep === '\\' ? p.replace(/\//g, '\\') : p.replace(/\\/g, '/');
  }

  // Convert slashes AND append a trailing separator. Used on blur / confirmed picks.
  function normalizeSep(p: string): string {
    const sep = $pathSep;
    const out = convertSep(p);
    return out && !out.endsWith(sep) ? out + sep : out;
  }

  function onPathInput(side: 'src' | 'dst') {
    const raw = side === 'src' ? $src : $dst;
    const converted = convertSep(raw);
    if (converted !== raw) {
      if (side === 'src') src.set(converted);
      else dst.set(converted);
    }
    scheduleComplete(converted, side);
  }

  function onPathKeydown(e: KeyboardEvent, side: 'src' | 'dst') {
    const completions = side === 'src' ? srcCompletions : dstCompletions;
    const highlight   = side === 'src' ? srcHighlight   : dstHighlight;
    const setHL = (h: number) => { if (side === 'src') srcHighlight = h; else dstHighlight = h; };
    const close = () => { if (side === 'src') srcDropdown = false; else dstDropdown = false; };

    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setHL(Math.min(highlight + 1, completions.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setHL(Math.max(highlight - 1, 0));
    } else if (e.key === 'Enter' && highlight >= 0) {
      e.preventDefault();
      pickCompletion(completions[highlight], side);
    } else if (e.key === 'Tab') {
      if (highlight >= 0 && completions.length > 0) {
        e.preventDefault();
        pickCompletion(completions[highlight], side);
        // don't also cycle focus: user just confirmed a completion
      } else {
        cyclePathFocus(e);
      }
    } else if (e.key === 'Escape') {
      close();
    }
  }

  function pickCompletion(path: string, side: 'src' | 'dst') {
    path = normalizeSep(path);
    if (side === 'src') {
      src.set(path);
      srcDropdown = false;
      srcCompletions = [];
    } else {
      dst.set(path);
      dstDropdown = false;
      dstCompletions = [];
    }
    // Re-run stat immediately for the chosen path
    scheduleStatCheck(path, side);
  }

  function onPathBlur(side: 'src' | 'dst') {
    // Append trailing separator now that the user has finished typing.
    const raw = side === 'src' ? $src : $dst;
    const normalized = normalizeSep(raw);
    if (normalized !== raw) {
      if (side === 'src') src.set(normalized);
      else dst.set(normalized);
    }
    // Delay dropdown close so a mousedown on a completion item fires first.
    setTimeout(() => {
      if (side === 'src') srcDropdown = false;
      else dstDropdown = false;
    }, 150);
  }

  $effect(() => { scheduleStatCheck($src, 'src'); });
  $effect(() => { scheduleStatCheck($dst, 'dst'); });

  const isBusy = $derived(status === 'running' || status === 'previewing' || previewing || running);
  const isPreviewing = $derived(status === 'previewing' || previewing);
  const isRunning = $derived(status === 'running' || running);
  const isPaused = $derived(status === 'paused');
  // Clear the "Pausing" transient label once the backend actually pauses (or stops).
  $effect(() => { if (isPaused || !isBusy) pausing = false; });
  const canPreview = $derived(!isBusy && !isPaused
    && srcExists === true          // SRC must exist and be a directory
    && dstExists === true);        // DST must exist and be a directory
  const canRun = $derived((status === 'idle' || status === 'done' || status === 'cancelled')
    && !running
    && !!$src.trim() && !!$dst.trim()
    && $planMeta.totalOps > 0);
  const canPause = $derived(isBusy || isPaused);
  const canCancel = $derived(isBusy || isPaused);

  function parentPath(p: string): string {
    const normalized = p.replace(/[\\/]+$/, '');
    const lastSep = Math.max(normalized.lastIndexOf('/'), normalized.lastIndexOf('\\'));
    if (lastSep < 0) return normalized;
    if (lastSep <= 2 && normalized[1] === ':') return normalized.slice(0, 2) + '\\';
    return normalized.slice(0, lastSep) || '/';
  }

  function normalizeBrowseResult(result: { path: string; entries: import('../lib/types').BrowseEntry[] }) {
    browsePath = normalizeSep(result.path);
    browseEntries = result.entries.map(e => ({ ...e, path: normalizeSep(e.path) }));
  }

  async function openBrowse(side: 'src' | 'dst') {
    browsingSide = side;
    const startPath = side === 'src' ? ($src || '') : ($dst || '');
    try {
      normalizeBrowseResult(await api.browse(startPath));
    } catch (err) {
      alert(`Browse failed: ${err}`);
      browsingSide = null;
    }
  }

  async function navigateBrowse(path: string) {
    try {
      normalizeBrowseResult(await api.browse(path));
    } catch (err) {
      alert(`Navigation failed: ${err}`);
    }
  }

  function selectBrowseDir(path: string) {
    path = normalizeSep(path);
    if (browsingSide === 'src') src.set(path);
    else dst.set(path);
    browsingSide = null;
  }

  async function toggleDark() {
    const newDark = !$isDark;
    isDark.set(newDark);
    const updated = { ...$config, theme: (newDark ? 'dark' : 'light') as import('../lib/types').Theme };
    config.set(updated);
    menuOpen = false;
    // Persist, or the choice is lost on restart: the store alone is not
    // written back to config.toml by anything else.
    try {
      await api.putConfig(updated);
    } catch (err) {
      console.error('Could not save theme:', err);
    }
  }

  function closeMenu() { menuOpen = false; }
</script>

<svelte:window onkeydown={(e) => { if (e.key === 'Escape' && browsingSide) browsingSide = null; }} />

<div class="topbar">
  <!-- Line 1: title + menu -->
  <div class="row row-1">
    <span class="app-title">
      <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="22" height="22" style="vertical-align:middle;border-radius:5px;margin-right:6px;">
        <rect width="100" height="100" rx="18" fill="#1E40AF"/>
        <path d="M8 24 L60 24 L60 16 L88 31 L60 46 L60 38 L8 38 Z" fill="white"/>
        <path d="M8 62 L60 62 L60 54 L88 69 L60 84 L60 76 L8 76 Z" fill="white"/>
      </svg>dirsync</span>
    <span class="drive-badge drive-badge-{driveMode}">{driveMode.toUpperCase()}</span>
    <div class="menu-wrap">
      <button type="button"class="menu-btn" onclick={() => menuOpen = !menuOpen} title="Menu">···</button>
      {#if menuOpen}
        <div class="backdrop" role="presentation" onclick={closeMenu}></div>
        <div class="dropdown">
          <button type="button" onclick={async () => { menuOpen = false; await api.shutdown().catch(() => {}); window.close(); }}>Close</button>
          <button type="button"onclick={toggleDark}>{$isDark ? '☀ Light mode' : '☾ Dark mode'}</button>
          <button type="button" onclick={() => { menuOpen = false; onshowLog(); }}>Log</button>
          <button type="button"onclick={() => { menuOpen = false; onshowLicenses(); }}>Licenses</button>
          <button type="button"onclick={() => { menuOpen = false; onshowAbout(); }}>About</button>
        </div>
      {/if}
    </div>
  </div>

  <!-- Line 2: SRC / DST paths -->
  <div class="row row-2">
    <div class="path-group">
      <span class="path-label">SRC</span>
      <div class="path-input-wrap">
        <input
          class="path-input"
          class:path-missing={srcExists === false}
          bind:value={$src}
          bind:this={srcInput}
          placeholder="/source/directory"
          autocomplete="off"
          oninput={() => onPathInput('src')}
          onkeydown={(e) => onPathKeydown(e, 'src')}
          onblur={() => onPathBlur('src')}
        />
        {#if srcDropdown}
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <div class="ac-dropdown" onmousedown={(e) => e.preventDefault()}>
            {#each srcCompletions as c, i}
              <button
                type="button"
                class="ac-item"
                class:ac-highlight={i === srcHighlight}
                onmouseenter={() => srcHighlight = i}
                onclick={() => pickCompletion(c, 'src')}
              >{c}</button>
            {/each}
          </div>
        {/if}
      </div>
      <button type="button" class="browse-btn" bind:this={srcBrowseBtn} onkeydown={cyclePathFocus} onclick={() => openBrowse('src')}>Browse…</button>
    </div>
    <div class="path-group">
      <span class="path-label">DST</span>
      <div class="path-input-wrap">
        <input
          class="path-input"
          class:path-missing={dstExists === false}
          bind:value={$dst}
          bind:this={dstInput}
          placeholder="/destination/directory"
          autocomplete="off"
          oninput={() => onPathInput('dst')}
          onkeydown={(e) => onPathKeydown(e, 'dst')}
          onblur={() => onPathBlur('dst')}
        />
        {#if dstDropdown}
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <div class="ac-dropdown" onmousedown={(e) => e.preventDefault()}>
            {#each dstCompletions as c, i}
              <button
                type="button"
                class="ac-item"
                class:ac-highlight={i === dstHighlight}
                onmouseenter={() => dstHighlight = i}
                onclick={() => pickCompletion(c, 'dst')}
              >{c}</button>
            {/each}
          </div>
        {/if}
      </div>
      <button type="button" class="browse-btn" bind:this={dstBrowseBtn} onkeydown={cyclePathFocus} onclick={() => openBrowse('dst')}>Browse…</button>
    </div>
  </div>

  <!-- Line 3: actions -->
  <div class="row row-3">
    <button type="button"class="action-btn primary" disabled={!canPreview} onclick={onpreview}>
      {#if isPreviewing}<span class="btn-spinner">⟳</span> Scanning…{:else}Preview{/if}
    </button>
    <button type="button" class="action-btn success" disabled={!canRun} onclick={onrun}>
      {#if isRunning && status !== 'running'}<span class="btn-spinner">⟳</span> Running…{:else}Run{/if}
    </button>
    <button type="button" class="action-btn warn" disabled={!canPause} onclick={() => { if (!isPaused) pausing = true; onpause(); }}>
      {isPaused ? 'Resume' : pausing ? 'Pausing…' : 'Pause'}
    </button>
    <button type="button"class="action-btn danger" disabled={!canCancel} onclick={oncancel}>
      Cancel
    </button>
    {#if status !== 'idle' || previewing || running}
      {@const displayStatus = (status === 'idle' && previewing) ? 'previewing' : (status === 'idle' && running) ? 'running' : status}
      <span class="status-badge status-{displayStatus}">{displayStatus}</span>
    {/if}
  </div>
</div>

<!-- Browse dialog -->
{#if browsingSide}
  <div class="overlay" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) browsingSide = null; }}>
    <div class="browse-dialog">
      <div class="browse-header">
        <span class="browse-title">Select {browsingSide.toUpperCase()} directory</span>
        <button type="button"class="close-btn" onclick={() => browsingSide = null}>✕</button>
      </div>
      <div class="browse-path-bar">
        <input class="browse-path-input" bind:value={browsePath} onchange={() => navigateBrowse(browsePath)} />
      </div>
      <div class="browse-list">
        {#if browsePath}
          <button type="button" class="browse-entry dir" onclick={() => navigateBrowse(parentPath(browsePath))}>
            <span class="entry-icon">📁</span>..
          </button>
        {/if}
        {#each browseEntries as entry}
          <button
            type="button"
            class="browse-entry"
            class:dir={entry.is_dir}
            onclick={() => entry.is_dir ? navigateBrowse(entry.path) : null}
          >
            <span class="entry-icon">{entry.is_dir ? '📁' : '📄'}</span>
            {entry.name}
          </button>
        {/each}
      </div>
      <div class="browse-footer">
        <button type="button"class="action-btn primary" onclick={() => selectBrowseDir(browsePath)}>
          Select "{browsePath.split(/[\\/]/).pop() || browsePath}"
        </button>
        <button type="button"class="action-btn" onclick={() => browsingSide = null}>Cancel</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .topbar {
    border-bottom: 1px solid var(--border);
    background: var(--surface);
    flex-shrink: 0;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
  }
  .row + .row { border-top: 1px solid var(--border-subtle); }

  /* Row 1 */
  .app-title {
    font-size: 15px;
    font-weight: 700;
    color: var(--text);
    flex: 1;
    letter-spacing: -0.3px;
  }
  .drive-badge {
    font-size: 10px;
    font-weight: 700;
    padding: 2px 7px;
    border-radius: 10px;
    letter-spacing: 0.5px;
  }
  .drive-badge-auto { background: var(--btn-bg); color: var(--text-muted); border: 1px solid var(--border); }
  .drive-badge-ssd  { background: #dbeafe; color: #1d4ed8; border: 1px solid #bfdbfe; }
  .drive-badge-hdd  { background: #fef9c3; color: #854d0e; border: 1px solid #fde68a; }

  .menu-wrap { position: relative; }
  .menu-btn {
    background: none;
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 3px 10px;
    cursor: pointer;
    font-size: 16px;
    color: var(--text);
    letter-spacing: 2px;
  }
  .menu-btn:hover { background: var(--hover); }
  .backdrop { position: fixed; inset: 0; z-index: 49; }
  .dropdown {
    position: absolute;
    right: 0;
    top: 100%;
    margin-top: 4px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.15);
    z-index: 50;
    display: flex;
    flex-direction: column;
    min-width: 160px;
    overflow: hidden;
  }
  .dropdown button {
    background: none;
    border: none;
    text-align: left;
    padding: 9px 16px;
    cursor: pointer;
    font-size: 13px;
    color: var(--text);
  }
  .dropdown button:hover { background: var(--hover); }

  /* Row 2 */
  .path-group {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .path-label {
    font-size: 11px;
    font-weight: 700;
    color: var(--text-muted);
    width: 28px;
  }
  .path-input-wrap {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    position: relative;
  }
  .path-input {
    width: 100%;
    box-sizing: border-box;
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 5px 8px;
    font-size: 12px;
    font-family: var(--font-mono);
    color: var(--text);
    outline: none;
  }
  .path-input:focus { border-color: var(--accent-blue); }
  .path-input.path-missing { color: var(--accent-red); border-color: var(--accent-red); }

  .ac-dropdown {
    position: absolute;
    top: calc(100% + 2px);
    left: 0;
    right: 0;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.15);
    z-index: 200;
    max-height: 220px;
    overflow-y: auto;
  }
  .ac-item {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    padding: 5px 10px;
    font-size: 12px;
    font-family: var(--font-mono);
    color: var(--text);
    cursor: pointer;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ac-item.ac-highlight { background: var(--accent-blue); color: #fff; }
  .browse-btn {
    background: none;
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 5px 10px;
    cursor: pointer;
    font-size: 12px;
    color: var(--text);
    white-space: nowrap;
  }
  .browse-btn:hover { background: var(--hover); }

  /* Row 3 */
  .action-btn {
    background: var(--btn-bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 5px 14px;
    cursor: pointer;
    font-size: 12px;
    font-weight: 500;
    color: var(--text);
    transition: opacity 0.1s;
  }
  .btn-spinner {
    display: inline-block;
    animation: spin 1s linear infinite;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .action-btn:disabled { opacity: 0.35; cursor: not-allowed; }
  .action-btn:not(:disabled):hover { opacity: 0.8; }
  .action-btn.primary { background: var(--accent-blue); color: #fff; border-color: var(--accent-blue); }
  .action-btn.success { background: var(--accent-green); color: #fff; border-color: var(--accent-green); }
  .action-btn.warn    { background: var(--accent-yellow); color: #333; border-color: var(--accent-yellow); }
  .action-btn.danger  { background: var(--accent-red); color: #fff; border-color: var(--accent-red); }

  .status-badge {
    font-size: 11px;
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }
  .status-running    { background: #d4f7e0; color: #15803d; }
  .status-previewing { background: #dbeafe; color: #1d4ed8; }
  .status-paused     { background: #fef9c3; color: #854d0e; }
  .status-done       { background: #d4f7e0; color: #15803d; }
  .status-cancelled  { background: #fee2e2; color: #b91c1c; }

  /* Browse overlay */
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0,0,0,0.4);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }
  .browse-dialog {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    width: min(560px, 90vw);
    max-height: 70vh;
    display: flex;
    flex-direction: column;
    box-shadow: 0 8px 32px rgba(0,0,0,0.2);
  }
  .browse-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 14px 16px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .browse-title { font-size: 14px; font-weight: 600; color: var(--text); }
  .close-btn {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 15px;
    color: var(--text-muted);
    padding: 3px 7px;
    border-radius: 4px;
  }
  .close-btn:hover { background: var(--hover); }
  .browse-path-bar {
    padding: 8px 12px;
    border-bottom: 1px solid var(--border-subtle);
    flex-shrink: 0;
  }
  .browse-path-input {
    width: 100%;
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 5px 8px;
    font-size: 12px;
    font-family: var(--font-mono);
    color: var(--text);
    box-sizing: border-box;
  }
  .browse-list {
    flex: 1;
    overflow-y: auto;
  }
  .browse-entry {
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    font-size: inherit;
    font-family: inherit;
    color: inherit;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 7px 16px;
    font-size: 13px;
    color: var(--text);
    border-bottom: 1px solid var(--border-subtle);
    cursor: default;
  }
  .browse-entry.dir { cursor: pointer; }
  .browse-entry.dir:hover { background: var(--hover); }
  .entry-icon { font-size: 14px; }
  .browse-footer {
    display: flex;
    gap: 8px;
    padding: 10px 12px;
    border-top: 1px solid var(--border);
    flex-shrink: 0;
  }
</style>
