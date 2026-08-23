<script lang="ts">
  import { licenses } from '../../lib/licenses_generated';
  const { onclose }: { onclose: () => void } = $props();

  // Map SPDX license IDs to their canonical license text URLs.
  const SPDX_URLS: Record<string, string> = {
    'MIT':              'https://spdx.org/licenses/MIT.html',
    'Apache-2.0':       'https://spdx.org/licenses/Apache-2.0.html',
    'ISC':              'https://spdx.org/licenses/ISC.html',
    'BSD-2-Clause':     'https://spdx.org/licenses/BSD-2-Clause.html',
    'BSD-3-Clause':     'https://spdx.org/licenses/BSD-3-Clause.html',
    'MPL-2.0':          'https://spdx.org/licenses/MPL-2.0.html',
    'GPL-2.0':          'https://spdx.org/licenses/GPL-2.0-only.html',
    'GPL-2.0-only':     'https://spdx.org/licenses/GPL-2.0-only.html',
    'GPL-2.0-or-later': 'https://spdx.org/licenses/GPL-2.0-or-later.html',
    'GPL-3.0':          'https://spdx.org/licenses/GPL-3.0-only.html',
    'GPL-3.0-only':     'https://spdx.org/licenses/GPL-3.0-only.html',
    'GPL-3.0-or-later': 'https://spdx.org/licenses/GPL-3.0-or-later.html',
    'LGPL-2.1':         'https://spdx.org/licenses/LGPL-2.1-only.html',
    'LGPL-3.0':         'https://spdx.org/licenses/LGPL-3.0-only.html',
    'AGPL-3.0':         'https://spdx.org/licenses/AGPL-3.0-only.html',
    'CC0-1.0':          'https://spdx.org/licenses/CC0-1.0.html',
    'CC-BY-4.0':        'https://spdx.org/licenses/CC-BY-4.0.html',
    'Unlicense':        'https://spdx.org/licenses/Unlicense.html',
    'Zlib':             'https://spdx.org/licenses/Zlib.html',
    '0BSD':             'https://spdx.org/licenses/0BSD.html',
    'BlueOak-1.0.0':    'https://spdx.org/licenses/BlueOak-1.0.0.html',
    'Python-2.0':       'https://spdx.org/licenses/Python-2.0.html',
    'Unicode-DFS-2016': 'https://spdx.org/licenses/Unicode-DFS-2016.html',
  };

  // Split a compound SPDX expression into individual identifiers.
  // Handles: "MIT OR Apache-2.0", "MIT/Apache-2.0", "MIT AND Apache-2.0"
  function splitLicense(expr: string): string[] {
    return expr.split(/\s+(?:OR|AND|WITH)\s+|\//).map(s => s.trim()).filter(Boolean);
  }

  interface LicensePart { id: string; url: string | null }
  function licenseparts(expr: string): LicensePart[] {
    return splitLicense(expr).map(id => ({ id, url: SPDX_URLS[id] ?? null }));
  }
</script>

<svelte:window onkeydown={(e) => { if (e.key === 'Escape') onclose(); }} />

<div class="overlay" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) onclose(); }}>
  <div class="dialog">
    <div class="dialog-header">
      <h2>Third-Party Licenses</h2>
      <button class="close-btn" onclick={onclose}>✕</button>
    </div>
    <div class="license-list">
      <div class="license-row header-row">
        <div class="l-name">Package</div>
        <div class="l-version">Version</div>
        <div class="l-license">License</div>
        <div class="l-copy">Copyright</div>
        <div></div>
      </div>
      {#each licenses as l}
        <div class="license-row">
          <div class="l-name">{l.name}</div>
          <div class="l-version">{l.version}</div>
          <div class="l-license">
            {#each licenseparts(l.license) as part, i}
              {#if i > 0}<span class="l-sep"> / </span>{/if}
              {#if part.url}
                <a class="l-lic-link" href={part.url} target="_blank" rel="noopener">{part.id}</a>
              {:else}
                <span>{part.id}</span>
              {/if}
            {/each}
          </div>
          <div class="l-copy">{l.copyright}</div>
          {#if l.url}
            <a class="l-link" href={l.url} target="_blank" rel="noopener" title="View repository">↗</a>
          {:else}
            <div></div>
          {/if}
        </div>
      {/each}
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
    width: min(960px, 96vw);
    max-height: 80vh;
    display: flex;
    flex-direction: column;
    box-shadow: 0 8px 32px rgba(0,0,0,0.2);
  }
  .dialog-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 16px 20px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  h2 { margin: 0; font-size: 16px; color: var(--text); }
  .close-btn {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 16px;
    color: var(--text-muted);
    padding: 4px 8px;
    border-radius: 4px;
  }
  .close-btn:hover { background: var(--hover); }
  .license-list {
    overflow-y: scroll;
    flex: 1;
    min-height: 0;
    padding: 0;
  }
  .license-row {
    display: grid;
    grid-template-columns: 180px 72px 150px 1fr 28px;
    align-items: center;
    gap: 8px;
    padding: 7px 20px;
    border-bottom: 1px solid var(--border-subtle);
    font-size: 12px;
  }
  .header-row {
    position: sticky;
    top: 0;
    background: var(--header-bg);
    border-bottom: 1px solid var(--border);
    font-weight: 700;
    font-size: 11px;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.4px;
    z-index: 1;
  }
  .license-row:not(.header-row):hover { background: var(--hover); }
  .l-name { font-weight: 600; color: var(--text); font-family: var(--font-mono); }
  .l-version { color: var(--text-muted); font-family: var(--font-mono); }
  .l-license { color: var(--text); display: flex; align-items: center; flex-wrap: wrap; gap: 0; }
  .l-lic-link { color: var(--accent-blue); text-decoration: none; }
  .l-lic-link:hover { text-decoration: underline; }
  .l-sep { color: var(--text-muted); }
  .l-copy { color: var(--text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .l-link { color: var(--accent-blue); text-decoration: none; text-align: center; }
  .l-link:hover { text-decoration: underline; }
</style>
