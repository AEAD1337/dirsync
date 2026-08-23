import type { AppConfig, BrowseEntry, LogEntry, PlanSummary, WsEvent } from './types';

const BASE = '/api/v1';

/// Error carrying the HTTP status so callers can distinguish expected
/// conflicts (409 while a run/preview is active) from real failures.
export class ApiError extends Error {
  constructor(
    message: string,
    public readonly status: number
  ) {
    super(message);
  }
}

async function request<T>(
  method: string,
  path: string,
  body?: unknown
): Promise<T> {
  const res = await fetch(BASE + path, {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) {
    const text = await res.text();
    throw new ApiError(`${method} ${path} → ${res.status}: ${text}`, res.status);
  }
  if (res.status === 204 || res.status === 202) return undefined as T;
  return res.json();
}

export const api = {
  getConfig: () => request<AppConfig>('GET', '/config'),
  putConfig: (cfg: AppConfig) => request<AppConfig>('PUT', '/config', cfg),

  preview: (src: string, dst: string, excludes: string[]) =>
    request<void>('POST', '/preview', { src, dst, excludes }),

  // src/dst are echoed so the server can reject a run whose stored plan no
  // longer matches what the user is looking at.
  run: (dry_run: boolean, skip_prefixes: string[], src: string, dst: string) =>
    request<void>('POST', '/run', { dry_run, skip_prefixes, src, dst }),
  pause: () => request<{ paused: boolean }>('POST', '/pause'),
  cancel: () => request<void>('POST', '/cancel'),

  getPlan: () => request<PlanSummary>('GET', '/plan'),

  browse: (path: string) =>
    request<{ path: string; entries: BrowseEntry[] }>('POST', '/browse', { path, dir_only: true }),

  stat: (path: string) =>
    request<{ exists: boolean; is_dir: boolean }>('POST', '/stat', { path }),

  complete: (path: string) =>
    request<{ completions: string[] }>('POST', '/complete', { path }),

  system: () => request<{ path_sep: string; auto_preview: boolean }>('GET', '/system'),

  shutdown: () => request<void>('POST', '/shutdown'),

  getLog: () => request<LogEntry[]>('GET', '/log'),
};

export class SyncWebSocket {
  private ws: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectAttempts = 0;
  onEvent: ((e: WsEvent) => void) | null = null;

  connect() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const url = `${proto}://${location.host}/ws`;
    this.ws = new WebSocket(url);

    this.ws.onopen = () => {
      this.reconnectAttempts = 0;
    };

    this.ws.onmessage = (msg) => {
      try {
        const event: WsEvent = JSON.parse(msg.data);
        this.onEvent?.(event);
      } catch {
        // ignore malformed
      }
    };

    this.ws.onclose = () => {
      this.scheduleReconnect();
    };

    this.ws.onerror = () => {
      // Detach onclose first so the close below doesn't double-schedule.
      if (this.ws) {
        this.ws.onclose = null;
        this.ws.close();
        this.ws = null;
        this.scheduleReconnect();
      }
    };
  }

  // One backoff implementation for both the error and close paths: the two
  // must never drift apart or reconnect behavior depends on how the socket died.
  private scheduleReconnect() {
    if (this.reconnectTimer) return; // already scheduled
    const delay = Math.min(500 * Math.pow(1.5, this.reconnectAttempts), 30000);
    this.reconnectAttempts++;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }

  disconnect() {
    if (this.reconnectTimer) { clearTimeout(this.reconnectTimer); this.reconnectTimer = null; }
    if (this.ws) {
      this.ws.onclose = null; // prevent scheduling a reconnect on intentional disconnect
      this.ws.close();
      this.ws = null;
    }
  }
}
