// Path helpers for the user-facing SRC/DST strings. The server OS decides the
// separator (`pathSep` store): '\\' on Windows, '/' elsewhere.

/** Convert slashes to the native separator without appending one (used while typing). */
export function convertSep(p: string, sep: string): string {
  return sep === '\\' ? p.replace(/\//g, '\\') : p.replace(/\\/g, '/');
}

/** Convert slashes AND append a trailing separator (used on blur and confirmed picks). */
export function normalizeSep(p: string, sep: string): string {
  const out = convertSep(p, sep);
  return out && !out.endsWith(sep) ? out + sep : out;
}

/**
 * True at a filesystem root, where there is no parent to go up to: `/`,
 * `C:\`, `C:/`, a bare `C:`, or a UNC share root `\\server\share`.
 */
export function isRootPath(p: string): boolean {
  return /^[\\/]+$/.test(p)
    || /^[A-Za-z]:[\\/]*$/.test(p)
    || /^[\\/]{2}[^\\/]+[\\/]+[^\\/]+[\\/]*$/.test(p);
}

/**
 * The parent directory of `p`, or `p` itself at a root. Stripping the last
 * segment naively turned `C:\` into the drive-relative `C:` (which lists the
 * process's cwd on that drive) and `/` into the empty string (which the
 * server reads as the home directory).
 */
export function parentPath(p: string): string {
  if (isRootPath(p)) return p;
  const trimmed = p.replace(/[\\/]+$/, '');
  const cut = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  if (cut < 0) return p; // a single relative segment: nothing above it to name
  // Keep the separator when the parent is a root ("C:\", "/", "\\srv\share\").
  const head = trimmed.slice(0, cut + 1);
  return isRootPath(head) ? head : trimmed.slice(0, cut);
}
