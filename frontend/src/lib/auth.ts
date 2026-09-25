// Per-process access token. The server opens the browser at
// `http://127.0.0.1:PORT/#t=<token>`; every API call and the WebSocket must
// present it, or the server answers 401. The fragment never reaches the
// server or its logs, and sessionStorage keeps it across a reload (F5) of
// this tab only, so a copied URL without the fragment is not authorized.

const STORAGE_KEY = 'dirsync-token';

let token: string | null = null;

function readStored(): string | null {
  try {
    return sessionStorage.getItem(STORAGE_KEY);
  } catch {
    return null; // storage disabled: the in-memory copy still covers this load
  }
}

function store(value: string) {
  try {
    sessionStorage.setItem(STORAGE_KEY, value);
  } catch {
    /* see readStored */
  }
}

/**
 * Pick up the token from `#t=...` (and strip it from the address bar, keeping
 * path and query) or fall back to the copy saved by an earlier load of this
 * tab. Must run before the first API call or WebSocket connect.
 */
export function initToken(): string | null {
  const hash = location.hash.startsWith('#') ? location.hash.slice(1) : location.hash;
  const fromHash = new URLSearchParams(hash).get('t');
  if (fromHash) {
    token = fromHash;
    store(fromHash);
    history.replaceState(history.state, '', location.pathname + location.search);
  } else {
    token = readStored();
  }
  return token;
}

export function getToken(): string | null {
  return token;
}
