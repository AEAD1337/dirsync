<script lang="ts">
  const {
    x = 0,
    y = 0,
    showSkip = false,
    onskip,
    onexclude,
    onclose,
  }: {
    x?: number;
    y?: number;
    showSkip?: boolean;
    onskip: () => void;
    onexclude: () => void;
    onclose: () => void;
  } = $props();

  function handle(action: 'skip' | 'exclude') {
    if (action === 'skip') onskip();
    else onexclude();
    onclose();
  }

  const MENU_W = 190;
  const ITEM_H = 35;
  const clampedX = $derived(Math.min(x, (typeof window !== 'undefined' ? window.innerWidth : 9999) - MENU_W - 8));
  const clampedY = $derived(Math.min(y, (typeof window !== 'undefined' ? window.innerHeight : 9999) - (showSkip ? 2 : 1) * ITEM_H - 24));
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && onclose()} />

<div class="backdrop" role="presentation" onclick={onclose}></div>

<menu class="ctx-menu" style="left:{clampedX}px; top:{clampedY}px">
  {#if showSkip}
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <li onclick={() => handle('skip')} onkeydown={() => {}}>Skip this directory</li>
  {/if}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <li onclick={() => handle('exclude')} onkeydown={() => {}}>Add exclusion pattern…</li>
</menu>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 99;
  }
  .ctx-menu {
    position: fixed;
    z-index: 100;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 4px 0;
    margin: 0;
    list-style: none;
    box-shadow: 0 4px 16px rgba(0,0,0,0.18);
    min-width: 180px;
  }
  .ctx-menu li {
    padding: 7px 14px;
    cursor: pointer;
    font-size: 13px;
    color: var(--text);
    white-space: nowrap;
  }
  .ctx-menu li:hover {
    background: var(--hover);
  }
</style>
