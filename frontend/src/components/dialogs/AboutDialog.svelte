<script lang="ts">
  const profile = typeof __BUILD_PROFILE__ !== 'undefined' ? __BUILD_PROFILE__ : 'debug';
  const version = (typeof __APP_VERSION__ !== 'undefined' ? __APP_VERSION__ : '0.1.0')
    + ` (${profile} build)`;
  // Matches the CLI's `--version` format ("%Y-%m-%d %H:%M UTC") rather than
  // showing the raw ISO timestamp Vite injects.
  function formatBuildTime(iso: string): string {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const pad = (n: number) => n.toString().padStart(2, '0');
    return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} `
      + `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())} UTC`;
  }
  const buildTime = typeof __BUILD_TIME__ !== 'undefined' ? formatBuildTime(__BUILD_TIME__) : 'dev';

  const { onclose }: { onclose: () => void } = $props();
</script>

<svelte:window onkeydown={(e) => { if (e.key === 'Escape') onclose(); }} />

<div class="overlay" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) onclose(); }}>
  <div class="dialog">
    <div class="logo-wrap">
      <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="64" height="64" style="border-radius:14px;">
        <rect width="100" height="100" rx="18" fill="#1E40AF"/>
        <path d="M8 24 L60 24 L60 16 L88 31 L60 46 L60 38 L8 38 Z" fill="white"/>
        <path d="M8 62 L60 62 L60 54 L88 69 L60 84 L60 76 L8 76 Z" fill="white"/>
      </svg>
    </div>
    <h2>dirsync</h2>
    <p>One-way directory mirror sync with smart rename/move detection.</p>
    <table>
      <tbody>
        <tr><td>Version</td><td>{version}</td></tr>
        <tr><td>Build time</td><td>{buildTime}</td></tr>
        <tr><td>License</td><td>GPL-3.0-only</td></tr>
      </tbody>
    </table>
    <button onclick={onclose}>Close</button>
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
    padding: 28px 32px;
    min-width: 300px;
    box-shadow: 0 8px 32px rgba(0,0,0,0.2);
  }
  .logo-wrap { text-align: center; margin-bottom: 16px; }
  h2 { margin: 0 0 8px; font-size: 18px; color: var(--text); text-align: center; }
  p  { margin: 0 0 16px; color: var(--text-muted); font-size: 13px; }
  table { border-collapse: collapse; width: 100%; margin-bottom: 20px; }
  td { padding: 4px 8px; font-size: 13px; color: var(--text); }
  td:first-child { font-weight: 600; color: var(--text-muted); width: 100px; }
  button {
    background: var(--accent-blue);
    color: #fff;
    border: none;
    border-radius: 6px;
    padding: 7px 18px;
    cursor: pointer;
    font-size: 13px;
  }
  button:hover { opacity: 0.85; }
</style>
