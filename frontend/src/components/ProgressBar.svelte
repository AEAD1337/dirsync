<script lang="ts">
  interface Props {
    value?: number;      // 0-100
    label?: string;
    visible?: boolean;
    color?: 'blue' | 'green';
    indeterminate?: boolean;
  }
  const { value = 0, label = '', visible = true, color = 'green', indeterminate = false }: Props = $props();
</script>

{#if visible}
  <div class="progress-wrap">
    <div class="progress-track">
      <div
        class="progress-fill {color}"
        style="width: {indeterminate ? 100 : Math.min(100, Math.max(0, value))}%"
        class:indeterminate
      ></div>
      <span class="progress-label">{label}</span>
    </div>
  </div>
{/if}

<style>
  .progress-wrap {
    padding: 2px 0;
  }
  .progress-track {
    position: relative;
    height: 22px;
    background: var(--bar-bg);
    border-radius: 4px;
    overflow: hidden;
  }
  .progress-fill {
    position: absolute;
    top: 0;
    left: 0;
    height: 100%;
    transition: width 0.15s ease;
    border-radius: 4px;
  }
  .progress-fill.green { background: var(--progress-green); }
  .progress-fill.blue  { background: var(--progress-blue); }
  .progress-label {
    position: absolute;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    font-size: 11px;
    font-weight: 600;
    color: var(--label-on-bar);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 90%;
    pointer-events: none;
  }
</style>
