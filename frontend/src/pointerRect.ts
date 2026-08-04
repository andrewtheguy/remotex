// The canvas client rect, cached for pointer-to-remote mapping.
//
// Reading `getBoundingClientRect` in every mousemove forces a synchronous
// layout flush at pointer rate, so the rect is cached. Two things end a cached
// rect's life, and both are needed:
//
// - `invalidate()`, called by everything known to move or resize the canvas —
//   `applyCanvasCss` (zoom, pan, a resize message, the soft-keyboard inset),
//   scroll, and a window resize. This is what keeps a pointer event *after* an
//   in-frame geometry change from mapping through the old rect: a resize
//   message and a mousemove are separate tasks and can share one frame.
// - the scheduled clear, one per cached read, as the backstop for whatever
//   moves the canvas without announcing it — nothing shows on screen sooner
//   than the frame boundary that clears this.
export interface RectCache {
  /** The target's client rect, fresh since the last geometry change. */
  read(target: Element): DOMRect;
  /** The canvas moved or resized: the next read must measure again. */
  invalidate(): void;
}

export function createRectCache(
  schedule: (clear: () => void) => void,
): RectCache {
  let rect: DOMRect | null = null;
  return {
    read(target) {
      if (!rect) {
        rect = target.getBoundingClientRect();
        schedule(() => {
          rect = null;
        });
      }
      return rect;
    },
    invalidate() {
      rect = null;
    },
  };
}
