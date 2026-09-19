<script lang="ts">
  import ProgressBar from './ProgressBar.svelte';
  import { progress, scanState } from '../lib/store';
  import { fmtCount, formatBytes, formatDuration, formatEta } from '../lib/store';
  import type { ProgressSnapshot } from '../lib/types';

  $: p = $progress;
  $: isPreviewing = $scanState.active;

  $: fileLabel = p.current_file
    ? `${p.current_file}  ${formatBytes(p.current_file_done)} / ${formatBytes(p.current_file_size)}`
    : '';

  $: overallPct = p.total_bytes > 0
    ? (p.done_bytes / p.total_bytes) * 100
    : (p.status === 'done' ? 100 : 0);

  $: overallLabel = `${overallPct.toFixed(1)}%`;

  // A file earns its own bar once what is left of it looks like more than
  // FILE_BAR_SECS at the current speed. Estimated rather than timed, so a slow
  // file gets its bar immediately instead of seconds in, and measured against
  // the remaining bytes so a nearly finished file does not qualify.
  const FILE_BAR_SECS = 3;
  // No speed sample yet (the first file of a run): the estimate has no
  // divisor, so fall back to plain size.
  const FILE_BAR_COLD_BYTES = 50 * 1024 * 1024;

  // The file the bar is currently shown for. Keeping it until the file changes
  // is what stops a wobbling speed sample flickering the bar on and off.
  let fileBarFile: string | null = null;

  function updateFileBar(p: ProgressSnapshot) {
    if (!p.current_file) {
      fileBarFile = null;
      return;
    }
    if (fileBarFile === p.current_file) return;
    const remaining = Math.max(p.current_file_size - p.current_file_done, 0);
    const bytesPerSec = p.speed_mbps * 1024 * 1024;
    const slow = bytesPerSec > 0
      ? remaining / bytesPerSec > FILE_BAR_SECS
      : p.current_file_size >= FILE_BAR_COLD_BYTES;
    if (slow) fileBarFile = p.current_file;
  }

  $: updateFileBar(p);
  $: showFileBar = !!p.current_file && fileBarFile === p.current_file;

  $: remaining = p.eta_secs != null ? formatDuration(p.eta_secs) : '-';
  $: eta = p.eta_secs != null ? formatEta(p.eta_secs) : '-';
</script>

<div class="bottom-bar">
  <div class="bars">
    {#if isPreviewing}
      <ProgressBar
        value={0}
        label="Scanning…"
        indeterminate={true}
        color="blue"
      />
    {:else}
      <ProgressBar
        value={p.current_file_pct}
        label={fileLabel}
        visible={showFileBar}
        color="blue"
      />
      <ProgressBar
        value={overallPct}
        label={overallLabel}
        color="green"
      />
    {/if}
  </div>
  <!-- Status bar order: Ops | Data | Elapsed | Remaining | ETA | Speed.
       Each value sits in a fixed-width slot so a shorter or longer reading
       never shifts the indicators to its right. -->
  <div class="indicators">
    <span class="ind"><span class="ind-label">Ops</span><span class="ind-value ops">{p.ops_total > 0 ? `${fmtCount(p.ops_done)}/${fmtCount(p.ops_total)}` : '-'}</span></span>
    <span class="ind"><span class="ind-label">Data</span><span class="ind-value data">{p.total_bytes > 0 ? `${formatBytes(p.done_bytes)}/${formatBytes(p.total_bytes)}` : '-'}</span></span>
    <span class="ind"><span class="ind-label">Elapsed</span><span class="ind-value time">{formatDuration(p.elapsed_secs)}</span></span>
    <span class="ind"><span class="ind-label">Remaining</span><span class="ind-value time">{remaining}</span></span>
    <span class="ind"><span class="ind-label">ETA</span><span class="ind-value eta">{eta}</span></span>
    <span class="ind"><span class="ind-label">Speed</span><span class="ind-value speed">{p.speed_mbps.toFixed(1)} MB/s</span></span>
  </div>
</div>

<style>
  .bottom-bar {
    border-top: 1px solid var(--border);
    padding: 8px 12px;
    background: var(--bar-bg-panel);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .bars {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .indicators {
    display: flex;
    gap: 20px;
    font-size: 11px;
    color: var(--text-muted);
    flex-wrap: wrap;
    /* Same advance width for every digit: without it 1 is narrower than 0,
       so even a same-length reading nudges its neighbours. */
    font-variant-numeric: tabular-nums;
  }
  .ind { display: flex; gap: 4px; align-items: baseline; }
  .ind-label { font-weight: 600; color: var(--text); }
  /* Widths cover the realistic worst case for each reading, so the row stays
     still as values grow and shrink. A value longer than its slot (a job of
     millions of ops, days of runtime) still grows rather than being clipped:
     reserving for those would waste most of the bar most of the time. */
  .ind-value { display: inline-block; }
  .ind-value.ops { min-width: 13ch; }     /* 999,999/999,999 */
  .ind-value.data { min-width: 17ch; }    /* 999.9 GB/999.9 GB */
  .ind-value.time { min-width: 10ch; }    /* 9h 59m 59s */
  .ind-value.eta { min-width: 8ch; }      /* Wed 10:47 */
  .ind-value.speed { min-width: 11ch; }   /* 1000.0 MB/s */
</style>
