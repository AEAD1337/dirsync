<script lang="ts">
  import ProgressBar from './ProgressBar.svelte';
  import { progress, scanState } from '../lib/store';
  import { fmtCount, formatBytes, formatDuration, formatEta } from '../lib/store';

  $: p = $progress;
  $: isPreviewing = $scanState.active;

  $: fileLabel = p.current_file
    ? `${p.current_file}  ${formatBytes(p.current_file_done)} / ${formatBytes(p.current_file_size)}`
    : '';

  $: overallPct = p.total_bytes > 0
    ? (p.done_bytes / p.total_bytes) * 100
    : (p.status === 'done' ? 100 : 0);

  $: overallLabel = `${overallPct.toFixed(1)}%`;

  // Show file bar only when a file is actively transferring
  $: showFileBar = !!p.current_file;

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
