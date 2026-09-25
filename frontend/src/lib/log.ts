import type { LogEntry } from './types';

/** Client log size: mirrors LOG_BUFFER_CAP in src/gui/ws.rs, the server's ring buffer. */
export const LOG_BUFFER_CAP = 2000;

function sameEntry(a: LogEntry, b: LogEntry): boolean {
  return a.level === b.level && a.message === b.message && a.run === b.run;
}

/** `list` followed by `more`, keeping only the newest LOG_BUFFER_CAP entries. */
export function appendLog(list: LogEntry[], more: LogEntry[]): LogEntry[] {
  if (more.length === 0) return list;
  const out = list.concat(more);
  return out.length > LOG_BUFFER_CAP ? out.slice(out.length - LOG_BUFFER_CAP) : out;
}

/**
 * Join the GET /log snapshot with the live entries the WebSocket delivered
 * while that request was in flight. The two overlap when an entry landed in
 * the server buffer before the snapshot was taken and was also broadcast:
 * entries carry no id, so the overlap is the longest prefix of `live` that
 * equals a suffix of `history`, and it is kept once.
 */
export function mergeLog(history: LogEntry[], live: LogEntry[]): LogEntry[] {
  let overlap = Math.min(history.length, live.length);
  for (; overlap > 0; overlap--) {
    const start = history.length - overlap;
    let match = true;
    for (let i = 0; i < overlap; i++) {
      if (!sameEntry(history[start + i], live[i])) { match = false; break; }
    }
    if (match) break;
  }
  return appendLog(history, live.slice(overlap));
}
