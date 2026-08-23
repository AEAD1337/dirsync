import type { OpEntry } from './types';

export type DisplayRow =
  | { rowType: 'dir'; path: string; name: string; depth: number }
  | { rowType: 'op'; path: string; name: string; depth: number; op: OpEntry & { error?: string } };

export type MergedRow = { src: DisplayRow | null; dst: DisplayRow | null };

/**
 * Sort key that orders a directory immediately before its own children.
 *
 * Plain string comparison puts `a/b.txt` before `a/b/x.txt` (because '.' 0x2E
 * sorts below '/' 0x2F), but `buildDisplayRows` emits the `a/b` directory row
 * only when it reaches `a/b/x.txt`: so the emitted rows come out as
 * `a`, `a/b.txt`, `a/b`, … which is not ascending. `mergeRows` is a merge join
 * and silently misaligns the two panels when its input is unsorted.
 *
 * Replacing the separator with NUL (below every printable character) makes a
 * directory sort before anything else sharing its stem, so the rows
 * `buildDisplayRows` emits are ascending by construction.
 */
export function pathKey(p: string): string {
  return p.replace(/\\/g, '/').replace(/\//g, '\x00');
}

/**
 * Build the flat list of rows to display in one tree panel.
 *
 * Assumes `ops` is already sorted by {@link pathKey} (App.svelte sorts once on
 * plan_ready). The algorithm is a single linear pass: it tracks the deepest
 * collapsed ancestor seen so far and skips every subsequent op whose path
 * starts with that prefix: no per-file O(depth²) ancestor scans needed.
 */
export function buildDisplayRows(
  ops: (OpEntry & { error?: string })[],
  collapsedSet: Set<string>,
): DisplayRow[] {
  const rows: DisplayRow[] = [];
  const seenDirs = new Set<string>();
  let collapsedPrefix: string | null = null;

  for (const op of ops) {
    // Normalise separators so all comparisons use '/'.
    const relPath = op.rel_path.replace(/\\/g, '/');
    const parts = relPath.split('/').filter(Boolean);
    if (parts.length === 0) continue;

    // If we're inside a collapsed subtree, skip until we exit it.
    if (collapsedPrefix !== null) {
      if (relPath.startsWith(collapsedPrefix + '/')) continue;
      collapsedPrefix = null;
    }

    // Emit ancestor directory rows; stop (and record the prefix) at the first
    // collapsed ancestor. Because ops are sorted, every op under that prefix
    // will be contiguous and skipped by the check above.
    let dirPath = '';
    let hitCollapsed = false;
    for (let i = 0; i < parts.length - 1; i++) {
      dirPath = i === 0 ? parts[0] : dirPath + '/' + parts[i];
      if (!seenDirs.has(dirPath)) {
        seenDirs.add(dirPath);
        rows.push({ rowType: 'dir', path: dirPath, name: parts[i], depth: i });
      }
      if (collapsedSet.has(dirPath)) {
        collapsedPrefix = dirPath;
        hitCollapsed = true;
        break;
      }
    }

    if (!hitCollapsed) {
      rows.push({
        rowType: 'op',
        path: op.rel_path,
        name: parts[parts.length - 1] ?? op.rel_path,
        depth: parts.length - 1,
        op,
      });
    }
  }

  return rows;
}

export function mergeRows(srcRows: DisplayRow[], dstRows: DisplayRow[]): MergedRow[] {
  const merged: MergedRow[] = [];
  let si = 0, di = 0;
  while (si < srcRows.length || di < dstRows.length) {
    const s = si < srcRows.length ? srcRows[si] : null;
    const d = di < dstRows.length ? dstRows[di] : null;
    if (!s) {
      merged.push({ src: null, dst: d });
      di++;
    } else if (!d) {
      merged.push({ src: s, dst: null });
      si++;
    } else {
      const sk = pathKey(s.path), dk = pathKey(d.path);
      const cmp = sk < dk ? -1 : sk > dk ? 1 : 0;
      if (cmp === 0) { merged.push({ src: s, dst: d }); si++; di++; }
      else if (cmp < 0) { merged.push({ src: s, dst: null }); si++; }
      else { merged.push({ src: null, dst: d }); di++; }
    }
  }
  return merged;
}
