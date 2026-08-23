<script lang="ts">
  import type { LogEntry } from '../../lib/types';
  import { tick } from 'svelte';

  const {
    entries,
    onclose,
    onclear,
  }: {
    entries: LogEntry[];
    onclose: () => void;
    onclear: () => void;
  } = $props();

  let listEl: HTMLDivElement = $state(undefined as any);

  // Auto-scroll to bottom when entries grow.
  $effect(() => {
    const _ = entries.length;
    tick().then(() => {
      if (listEl) listEl.scrollTop = listEl.scrollHeight;
    });
  });

  // Build display rows: inject a separator row whenever `run` changes.
  type Row =
    | { kind: 'entry'; entry: LogEntry }
    | { kind: 'separator'; run: number };

  function buildRows(entries: LogEntry[]): Row[] {
    const rows: Row[] = [];
    let lastRun = -1;
    for (const entry of entries) {
      if (entry.run !== lastRun) {
        if (lastRun !== -1) {
          rows.push({ kind: 'separator', run: entry.run });
        }
        lastRun = entry.run;
      }
      rows.push({ kind: 'entry', entry });
    }
    return rows;
  }

  const rows = $derived(buildRows(entries));
</script>

<svelte:window onkeydown={(e) => { if (e.key === 'Escape') onclose(); }} />

<div class="overlay" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) onclose(); }}>
  <div class="dialog">
    <div class="header">
      <span class="title">Log</span>
      <div class="header-actions">
        <button class="btn-clear" onclick={onclear} title="Clear log">Clear</button>
        <button class="btn-close" onclick={onclose} title="Close">✕</button>
      </div>
    </div>
    <div class="list" bind:this={listEl}>
      {#if rows.length === 0}
        <div class="empty">No log entries yet.</div>
      {:else}
        {#each rows as row}
          {#if row.kind === 'separator'}
            <div class="separator"><span>Run {row.run}</span></div>
          {:else}
            <div class="entry level-{row.entry.level}">{row.entry.message}</div>
          {/if}
        {/each}
      {/if}
    </div>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0,0,0,0.4);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 200;
  }

  .dialog {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    width: 80vw;
    height: 70vh;
    display: flex;
    flex-direction: column;
    box-shadow: 0 8px 32px rgba(0,0,0,0.2);
    overflow: hidden;
  }

  .header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .title {
    font-size: 14px;
    font-weight: 600;
    color: var(--text);
  }

  .header-actions {
    display: flex;
    gap: 8px;
    align-items: center;
  }

  .btn-clear {
    background: var(--btn-bg);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 4px 10px;
    font-size: 12px;
    cursor: pointer;
    color: var(--text-muted);
  }
  .btn-clear:hover { color: var(--text); }

  .btn-close {
    background: none;
    border: none;
    cursor: pointer;
    color: var(--text-muted);
    font-size: 14px;
    padding: 2px 6px;
  }
  .btn-close:hover { color: var(--text); }

  .list {
    flex: 1;
    overflow-y: auto;
    padding: 8px 0;
    font-family: var(--font-mono);
    font-size: 12px;
  }

  .entry {
    padding: 2px 16px;
    white-space: pre-wrap;
    word-break: break-all;
    color: var(--text);
    line-height: 1.5;
  }
  .entry.level-warning { color: var(--accent-yellow); }
  .entry.level-error   { color: var(--accent-red); }

  .separator {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 16px 6px;
    font-size: 11px;
    color: var(--text-muted);
    letter-spacing: 0.04em;
  }
  .separator::before,
  .separator::after {
    content: '';
    flex: 1;
    height: 1px;
    background: var(--border);
  }

  .empty {
    padding: 24px 16px;
    color: var(--text-muted);
    font-size: 12px;
    text-align: center;
  }
</style>
