import type { AppConfig, BrowseEntry, ConfigPatch, LogEntry, PlanSummary, WsEvent } from './types';
import { getToken } from './auth';
import { unauthorized } from './store';

const BASE = '/api/v1';
const TOKEN_HEADER = 'X-Dirsync-Token';

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

/// True for a 401: the page holds no valid token. The persistent banner
/// already says so, so callers skip their own alert for it.
export function isAuthError(err: unknown): boolean {
  return err instanceof ApiError && err.status === 401;
}

async function send(method: string, path: string, body?: unknown): Promise<Response> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers[TOKEN_HEADER] = token;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const res = await fetch(BASE + path, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) {
    if (res.status === 401) unauthorized.set(true);
    const text = await res.text();
    throw new ApiError(`${method} ${path} -> ${res.status}: ${text}`, res.status);
  }
  return res;
}

/// A request whose response body is the JSON value `T`.
async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await send(method, path, body);
  return (await res.json()) as T;
}

/// A request answered with 202/204 (or a body nobody reads).
async function requestVoid(method: string, path: string, body?: unknown): Promise<void> {
  await send(method, path, body);
}

export const api = {
  getConfig: () => request<AppConfig>('GET', '/config'),
  // Partial update; the server merges and returns the full config.
  putConfig: (patch: ConfigPatch) => request<AppConfig>('PUT', '/config', patch),

  preview: (src: string, dst: string, excludes: string[]) =>
    requestVoid('POST', '/preview', { src, dst, excludes }),

  // src/dst are echoed so the server can reject a run whose stored plan no
  // longer matches what the user is looking at.
  run: (dry_run: boolean, skip_prefixes: string[], src: string, dst: string) =>
    requestVoid('POST', '/run', { dry_run, skip_prefixes, src, dst }),
  // Toggles; the answer is the pause state the server now holds.
  pause: () => request<{ paused: boolean }>('POST', '/pause'),
  cancel: () => requestVoid('POST', '/cancel'),

  getPlan: () => request<PlanSummary>('GET', '/plan'),

  browse: (path: string) =>
    request<{ path: string; entries: BrowseEntry[] }>('POST', '/browse', { path, dir_only: true }),

  stat: (path: string) =>
    request<{ exists: boolean; is_dir: boolean }>('POST', '/stat', { path }),

  complete: (path: string) =>
    request<{ completions: string[] }>('POST', '/complete', { path }),

  system: () => request<{ path_sep: string; auto_preview: boolean }>('GET', '/system'),

  shutdown: () => requestVoid('POST', '/shutdown'),

  getLog: () => request<LogEntry[]>('GET', '/log'),
};

/// Minimal shape check: an object with a string `type`. Anything else is not
/// a server event and is dropped rather than cast blindly.
function asWsEvent(data: unknown): WsEvent | null {
  if (typeof data !== 'object' || data === null) return null;
  return typeof (data as { type?: unknown }).type === 'string' ? (data as WsEvent) : null;
}

export class SyncWebSocket {
  private ws: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectAttempts = 0;
  private stopped = false;
  onEvent: ((e: WsEvent) => void) | null = null;
  /// Called when there is no token at all: nothing can authorize this page.
  onNoToken: (() => void) | null = null;
  /// Asked before every reconnect; resolving false stops the loop (e.g. the
  /// server has answered 401, which retrying cannot fix).
  shouldReconnect: (() => Promise<boolean>) | null = null;

  connect() {
    const token = getToken();
    if (!token) {
      // No retry: without a token every attempt would be refused.
      this.stopped = true;
      this.onNoToken?.();
      return;
    }
    this.stopped = false;
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const url = `${proto}://${location.host}/ws?token=${encodeURIComponent(token)}`;
    this.ws = new WebSocket(url);

    this.ws.onopen = () => {
      this.reconnectAttempts = 0;
    };

    this.ws.onmessage = (msg) => {
      let event: WsEvent | null = null;
      try {
        event = asWsEvent(JSON.parse(msg.data));
      } catch {
        return; // not JSON
      }
      if (event) this.onEvent?.(event);
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
    if (this.stopped || this.reconnectTimer) return; // stopped, or already scheduled
    const delay = Math.min(500 * Math.pow(1.5, this.reconnectAttempts), 30000);
    this.reconnectAttempts++;
    this.reconnectTimer = setTimeout(async () => {
      this.reconnectTimer = null;
      if (this.shouldReconnect && !(await this.shouldReconnect())) return;
      // disconnect() may have run while shouldReconnect was pending.
      if (!this.stopped) this.connect();
    }, delay);
  }

  disconnect() {
    this.stopped = true;
    if (this.reconnectTimer) { clearTimeout(this.reconnectTimer); this.reconnectTimer = null; }
    if (this.ws) {
      this.ws.onclose = null; // prevent scheduling a reconnect on intentional disconnect
      this.ws.close();
      this.ws = null;
    }
  }
}
