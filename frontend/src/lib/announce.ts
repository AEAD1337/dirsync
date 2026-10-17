// Spoken status for screen readers: preview and run outcomes otherwise
// change only what is drawn. `document.ariaNotify` (Baseline since
// September 2026) needs no live-region element; older browsers lack it and
// simply stay silent. Typed locally rather than by augmenting `Document`, so
// a future lib.dom declaration of the same method cannot clash with it.
type AriaNotifyDocument = Document & {
  ariaNotify?: (announcement: string, options?: { priority?: 'normal' | 'high' }) => void;
};

export function announce(message: string): void {
  (document as AriaNotifyDocument).ariaNotify?.(message);
}
