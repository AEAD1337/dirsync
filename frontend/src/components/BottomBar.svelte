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
  <!-- Status bar order: Ops | Elapsed | Remaining | ETA | Speed -->
  <div class="indicators">
    <span class="ind"><span class="ind-label">Ops</span> {p.ops_total > 0 ? `${fmtCount(p.ops_done)}/${fmtCount(p.ops_total)}` : '-'}</span>
    <span class="ind"><span class="ind-label">Elapsed</span> {formatDuration(p.elapsed_secs)}</span>
    <span class="ind"><span class="ind-label">Remaining</span> {remaining}</span>
    <span class="ind"><span class="ind-label">ETA</span> {eta}</span>
    <span class="ind"><span class="ind-label">Speed</span> {p.speed_mbps.toFixed(1)} MB/s</span>
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
  }
  .ind { display: flex; gap: 4px; align-items: baseline; }
  .ind-label { font-weight: 600; color: var(--text); }
</style>
