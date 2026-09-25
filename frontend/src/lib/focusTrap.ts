import type { Action } from 'svelte/action';

// Shared modal keyboard behaviour for dialogs and the context menu: initial
// focus, Tab kept inside, Escape closes, focus handed back on close. Traps
// stack, so only the topmost one reacts. The key handling sits on window in
// the capture phase: it must win over the tree's global arrow/Enter handler
// and still work when a click on a non-focusable part of the dialog moved
// focus to <body>.

export interface TrapOptions {
  onclose: () => void;
  /** Menu mode: ArrowUp/ArrowDown/Home/End move between the items too. */
  arrows?: boolean;
}

interface Trap {
  node: HTMLElement;
  opts: TrapOptions;
}

const FOCUSABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(',');

const stack: Trap[] = [];

/** True while any dialog or menu holds the keyboard. */
export function isTrapActive(): boolean {
  return stack.length > 0;
}

function focusables(node: HTMLElement): HTMLElement[] {
  return Array.from(node.querySelectorAll<HTMLElement>(FOCUSABLE))
    .filter(el => el.getClientRects().length > 0);
}

function onKeydown(e: KeyboardEvent) {
  const top = stack[stack.length - 1];
  if (!top) return;

  if (e.key === 'Escape') {
    e.preventDefault();
    e.stopPropagation();
    top.opts.onclose();
    return;
  }

  const isArrow = top.opts.arrows
    && (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Home' || e.key === 'End');
  if (e.key !== 'Tab' && !isArrow) return;

  const items = focusables(top.node);
  if (items.length === 0) {
    e.preventDefault();
    top.node.focus();
    return;
  }
  const active = document.activeElement as HTMLElement | null;
  const i = active ? items.indexOf(active) : -1;
  const last = items.length - 1;
  let next: number;
  if (e.key === 'Tab') {
    if (i === -1) next = e.shiftKey ? last : 0;          // focus escaped: pull it back
    else if (e.shiftKey && i === 0) next = last;          // wrap backwards
    else if (!e.shiftKey && i === last) next = 0;         // wrap forwards
    else return;                                          // inside: browser order
  } else if (e.key === 'Home') next = 0;
  else if (e.key === 'End') next = last;
  else if (e.key === 'ArrowDown') next = i === -1 ? 0 : (i + 1) % items.length;
  else next = i === -1 ? last : (i - 1 + items.length) % items.length;
  e.preventDefault();
  e.stopPropagation();
  items[next].focus();
}

/**
 * `use:trapFocus={{ onclose }}` on the dialog (or menu) element. Initial focus
 * goes to the first `[data-autofocus]` descendant, else the first focusable
 * one, else the element itself (give it tabindex="-1").
 */
export const trapFocus: Action<HTMLElement, TrapOptions> = (node, opts) => {
  const trap: Trap = { node, opts: opts ?? { onclose: () => {} } };
  const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;

  stack.push(trap);
  if (stack.length === 1) window.addEventListener('keydown', onKeydown, true);

  const initial = node.querySelector<HTMLElement>('[data-autofocus]') ?? focusables(node)[0] ?? node;
  initial.focus();

  return {
    update(next: TrapOptions) {
      trap.opts = next;
    },
    destroy() {
      const idx = stack.indexOf(trap);
      if (idx !== -1) stack.splice(idx, 1);
      if (stack.length === 0) window.removeEventListener('keydown', onKeydown, true);
      // Hand focus back to what opened us, when it still exists.
      if (previous && previous.isConnected && previous !== document.body) previous.focus();
    },
  };
};
