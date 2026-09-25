import type { OpEntry } from './types';

/** Fixed height of every tree row in px: must match .tree-dir/.tree-row/.tree-gap in TreePanel.svelte. */
export const ROW_HEIGHT = 22;

/** A plan op carrying its precomputed {@link pathKey}. */
export type PlanOp = OpEntry & { key: string };

export type DisplayRow =
  | { rowType: 'dir'; path: string; key: string; name: string; depth: number }
  | { rowType: 'op'; path: string; key: string; name: string; depth: number; op: PlanOp };

export type MergedRow = { src: DisplayRow | null; dst: DisplayRow | null };

/**
 * Sort key that orders a directory immediately before its own children.
 *
 * Plain string comparison puts `a/b.txt` before `a/b/x.txt` (because '.' 0x2E
 * sorts below '/' 0x2F), but `buildDisplayRows` emits the `a/b` directory row
 * only when it reaches `a/b/x.txt`: so the emitted rows come out as
 * `a`, `a/b.txt`, `a/b`, ... which is not ascending. `mergeRows` is a merge join
 * and silently misaligns the two panels when its input is unsorted.
 *
 * Replacing the separator with NUL (below every printable character) makes a
 * directory sort before anything else sharing its stem, so the rows
 * `buildDisplayRows` emits are ascending by construction.
 */
export function pathKey(p: string): string {
  return p.replace(/[\\/]/g, '\x00');
}

/**
 * Decorate each op with its path key and sort by it: one key computation per
 * op instead of two per comparison (a 100k-op plan used to run millions of
 * regex replaces inside the comparator).
 */
export function sortOps(ops: OpEntry[]): PlanOp[] {
  const keyed: PlanOp[] = ops.map(op => ({ ...op, key: pathKey(op.rel_path) }));
  keyed.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
  return keyed;
}

/** Stable identity of a row within one panel: a dir row and a dir-rename op row can share a path. */
export function rowId(row: DisplayRow): string {
  return `${row.rowType}:${row.path}`;
}

/**
 * Build the flat list of rows to display in one tree panel.
 *
 * Assumes `ops` is sorted by key (see {@link sortOps}). The algorithm is a
 * single linear pass: it tracks the deepest collapsed ancestor seen so far and
 * skips every subsequent op whose key starts with that prefix: no per-file
 * O(depth^2) ancestor scans needed.
 */
export function buildDisplayRows(ops: PlanOp[], collapsedSet: Set<string>): DisplayRow[] {
  const rows: DisplayRow[] = [];
  const seenDirs = new Set<string>();
  let collapsedKey: string | null = null;

  for (const op of ops) {
    // The key is the path split on NUL: separators are already normalised.
    const parts = op.key.split('\x00').filter(Boolean);
    if (parts.length === 0) continue;

    // If we're inside a collapsed subtree, skip until we exit it.
    if (collapsedKey !== null) {
      if (op.key.startsWith(collapsedKey + '\x00')) continue;
      collapsedKey = null;
    }

    // Emit ancestor directory rows; stop (and record the prefix) at the first
    // collapsed ancestor. Because ops are sorted, every op under that prefix
    // will be contiguous and skipped by the check above.
    let dirPath = '';
    let dirKey = '';
    let hitCollapsed = false;
    for (let i = 0; i < parts.length - 1; i++) {
      dirPath = i === 0 ? parts[0] : dirPath + '/' + parts[i];
      dirKey = i === 0 ? parts[0] : dirKey + '\x00' + parts[i];
      if (!seenDirs.has(dirPath)) {
        seenDirs.add(dirPath);
        rows.push({ rowType: 'dir', path: dirPath, key: dirKey, name: parts[i], depth: i });
      }
      if (collapsedSet.has(dirPath)) {
        collapsedKey = dirKey;
        hitCollapsed = true;
        break;
      }
    }

    if (!hitCollapsed) {
      rows.push({
        rowType: 'op',
        path: op.rel_path,
        key: op.key,
        name: parts[parts.length - 1],
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
      const cmp = s.key < d.key ? -1 : s.key > d.key ? 1 : 0;
      if (cmp === 0) { merged.push({ src: s, dst: d }); si++; di++; }
      else if (cmp < 0) { merged.push({ src: s, dst: null }); si++; }
      else { merged.push({ src: null, dst: d }); di++; }
    }
  }
  return merged;
}
