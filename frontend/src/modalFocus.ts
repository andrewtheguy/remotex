// Keeping Tab inside a modal card. See ModalOverlay in FloatingMenu.tsx.

const FOCUSABLE =
  'a[href], button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

/** The parts of a focusable element this reads, so a test can hand it fakes. */
export interface Focusable {
  focus(): void;
}

/** The parts of the card this reads. */
export interface ModalCard extends Focusable {
  contains(node: unknown): boolean;
  querySelectorAll(selectors: string): ArrayLike<Focusable>;
}

/** The parts of a Tab keydown this reads. */
export interface TabKey {
  shiftKey: boolean;
  preventDefault(): void;
}

/**
 * Tab and Shift+Tab wrap around the card's own controls instead of leaving it for the
 * page behind the backdrop. `active` is the focused element. The card itself is
 * focused when it opens, so it counts as being before the first control and after
 * the last.
 */
export function keepTabWithin(
  card: ModalCard | null,
  active: unknown,
  e: TabKey,
): void {
  if (!card) {
    return;
  }
  const focusable = card.querySelectorAll(FOCUSABLE);
  const first = focusable[0];
  const last = focusable[focusable.length - 1];
  const atEdge = active === card || !card.contains(active);
  if (!first || !last) {
    e.preventDefault();
    card.focus();
  } else if (e.shiftKey && (active === first || atEdge)) {
    e.preventDefault();
    last.focus();
  } else if (!e.shiftKey && (active === last || atEdge)) {
    e.preventDefault();
    first.focus();
  }
}
