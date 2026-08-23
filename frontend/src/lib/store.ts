import { writable, derived } from 'svelte/store';
import type { AppConfig, OpEntry, ProgressSnapshot, ScanProgressPhase, SyncStatus } from './types';

export const config = writable<AppConfig>({
  port: 7373,
  exclude_patterns: [],
  last_src: null,
  last_dst: null,
  theme: 'light',
});

export const src = writable('');
export const dst = writable('');

export const progress = writable<ProgressSnapshot>({
  done_bytes: 0,
  total_bytes: 0,
  current_file: null,
  current_file_done: 0,
  current_file_size: 0,
  current_file_pct: 0,
  speed_mbps: 0,
  elapsed_secs: 0,
  eta_secs: null,
  ops_done: 0,
  ops_total: 0,
  status: 'idle',
});

export const status = derived(progress, ($p) => $p.status as SyncStatus);

// Ops list: all planned ops, removed on completion, kept on error
export const ops = writable<(OpEntry & { error?: string })[]>([]);

// Errors that occurred during sync
export const errors = writable<{ path: string; message: string }[]>([]);

export const isDark = writable(false);

// Native path separator for the server OS ('\\' on Windows, '/' on Linux/macOS).
export const pathSep = writable('/');

// Collapsed directory paths shared between the two tree panels.
export const collapsedDirs = writable(new Set<string>());

// Plan-level metadata set when plan_ready arrives, cleared on next preview.
export const planMeta = writable<{
  totalOps: number;
  totalBytes: number;
  srcDirSizes: Record<string, number>;
}>({ totalOps: 0, totalBytes: 0, srcDirSizes: {} });

// Scan progress state during Preview
export const scanState = writable<{
  active: boolean;
  src: number | null;   // file count after src walk, null = still scanning
  dst: number | null;
}>({ active: false, src: null, dst: null });

// Current scan phase details: src/dst walking paths tracked independently,
// plus a global phase for hashing/planning (shown on both panels).
export const scanProgress = writable<{
  srcPath: string | null;
  dstPath: string | null;
  globalPhase: 'hashing' | 'planning' | null;
  globalPath: string | null;
}>({ srcPath: null, dstPath: null, globalPhase: null, globalPath: null });

export function fmtCount(n: number): string {
  return n.toLocaleString('en-US');
}

export function formatBytes(bytes: number, decimals = 1): string {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  // Clamp: beyond TB the index runs off the end of `sizes` and renders
  // "1.0 undefined".
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(k)), sizes.length - 1);
  return `${(bytes / Math.pow(k, i)).toFixed(decimals)} ${sizes[i]}`;
}

export function formatDuration(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = Math.floor(secs % 60);
  if (h > 0) return `${h}h ${String(m).padStart(2, '0')}m ${String(s).padStart(2, '0')}s`;
  if (m > 0) return `${m}m ${String(s).padStart(2, '0')}s`;
  return `${s}s`;
}

/** Returns the wall-clock arrival time as HH:MM (or "Www HH:MM" if on a different day). */
export function formatEta(eta_secs: number): string {
  const arrival = new Date(Date.now() + eta_secs * 1000);
  const hh = String(arrival.getHours()).padStart(2, '0');
  const mm = String(arrival.getMinutes()).padStart(2, '0');
  if (arrival.toDateString() === new Date().toDateString()) {
    return `${hh}:${mm}`;
  }
  const days = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
  return `${days[arrival.getDay()]} ${hh}:${mm}`;
}
