import { rowId, type DisplayRow } from './treeUtils';

// Keyboard navigation over one panel's rows. Pure: App.svelte owns the state
// and applies the returned action. `null` entries are the alignment gaps
// mergeRows inserts opposite a row of the other panel; they are never focused.

export type Rows = (DisplayRow | null)[];

/** Focus is tracked by row identity, so it survives rows shifting as ops complete. */
export interface FocusRef {
  id: string | null;
  /** Last known index: where to land when the focused row itself goes away. */
  index: number;
}

export const NO_FOCUS: FocusRef = { id: null, index: -1 };

export type NavAction =
  | { kind: 'focus'; index: number }
  | { kind: 'collapse' | 'expand' | 'toggle'; path: string };

export const TREE_KEYS = new Set([
  'ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End',
  'ArrowLeft', 'ArrowRight', ' ', 'Enter',
]);

export function focusRefAt(rows: Rows, index: number): FocusRef {
  const row = rows[index];
  return row ? { id: rowId(row), index } : NO_FOCUS;
}

/** Index of the focused row, or -1 when it is no longer displayed. */
export function resolveFocus(rows: Rows, ref: FocusRef): number {
  if (ref.id === null) return -1;
  const at = rows[ref.index];
  if (at && rowId(at) === ref.id) return ref.index; // fast path: nothing shifted
  return rows.findIndex(r => r !== null && rowId(r) === ref.id);
}

/** The non-gap row nearest to `index` (searching down first), or -1 when there is none. */
export function nearestRow(rows: Rows, index: number): number {
  if (rows.length === 0) return -1;
  const start = Math.max(0, Math.min(index, rows.length - 1));
  for (let d = 0; d < rows.length; d++) {
    if (start + d < rows.length && rows[start + d] !== null) return start + d;
    if (start - d >= 0 && rows[start - d] !== null) return start - d;
  }
  return -1;
}

function step(rows: Rows, from: number, dir: 1 | -1): number | null {
  let next = from + dir;
  while (next >= 0 && next < rows.length && rows[next] === null) next += dir;
  return next >= 0 && next < rows.length ? next : null;
}

/**
 * The action for `key` with row `index` focused (-1: none), or null when the
 * key does nothing here. `pageRows` is how many rows one PageUp/PageDown moves.
 */
export function treeNav(
  key: string,
  rows: Rows,
  index: number,
  pageRows: number,
  isCollapsed: (path: string) => boolean,
): NavAction | null {
  if (rows.length === 0) return null;
  const focus = (i: number | null): NavAction | null =>
    i === null || i < 0 ? null : { kind: 'focus', index: i };

  switch (key) {
    case 'ArrowUp':
    case 'ArrowDown': {
      const dir = key === 'ArrowUp' ? -1 : 1;
      if (index === -1) return focus(dir === 1 ? step(rows, -1, 1) : step(rows, rows.length, -1));
      return focus(step(rows, index, dir));
    }
    case 'PageUp':
    case 'PageDown': {
      const dir = key === 'PageUp' ? -1 : 1;
      const target = index === -1
        ? (dir === 1 ? 0 : rows.length - 1)
        : Math.max(0, Math.min(rows.length - 1, index + dir * pageRows));
      return focus(nearestRow(rows, target));
    }
    case 'Home':
      return focus(step(rows, -1, 1));
    case 'End':
      return focus(step(rows, rows.length, -1));
    case 'ArrowLeft':
    case 'ArrowRight':
    case ' ':
    case 'Enter': {
      const row = index >= 0 ? rows[index] : null;
      if (!row) return null;
      if (key === 'ArrowLeft') {
        // Expanded dir: collapse it and stay. Collapsed dir or file: go to the parent.
        if (row.rowType === 'dir' && !isCollapsed(row.path)) return { kind: 'collapse', path: row.path };
        for (let i = index - 1; i >= 0; i--) {
          const r = rows[i];
          if (r && r.rowType === 'dir' && row.path.startsWith(r.path + '/')) return { kind: 'focus', index: i };
        }
        return null;
      }
      if (row.rowType !== 'dir') return null;
      return key === 'ArrowRight' ? { kind: 'expand', path: row.path } : { kind: 'toggle', path: row.path };
    }
  }
  return null;
}
