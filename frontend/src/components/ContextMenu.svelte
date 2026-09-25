<script lang="ts">
  import { trapFocus } from '../lib/focusTrap';

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

<div class="backdrop" role="presentation" onclick={onclose}></div>

<!-- The first item takes focus; arrows move, Escape closes (trapFocus). -->
<div
  class="ctx-menu"
  role="menu"
  aria-label="Row actions"
  tabindex="-1"
  style="left:{clampedX}px; top:{clampedY}px"
  use:trapFocus={{ onclose, arrows: true }}
>
  {#if showSkip}
    <button type="button" role="menuitem" onclick={() => handle('skip')}>Skip this directory</button>
  {/if}
  <button type="button" role="menuitem" onclick={() => handle('exclude')}>Add exclusion pattern...</button>
</div>

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
    display: flex;
    flex-direction: column;
    box-shadow: 0 4px 16px rgba(0,0,0,0.18);
    min-width: 180px;
  }
  .ctx-menu button {
    background: none;
    border: none;
    text-align: left;
    font-family: inherit;
    padding: 7px 14px;
    cursor: pointer;
    font-size: 13px;
    color: var(--text);
    white-space: nowrap;
  }
  .ctx-menu button:hover,
  .ctx-menu button:focus-visible {
    background: var(--hover);
    outline: none;
  }
</style>
