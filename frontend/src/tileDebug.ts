// TESTING ONLY: the debug overlay of a picture sent as tiles (`png_tiles =
// "debug"` on an RDP target's bitmap path, announced by `tilesStart`).
//
// The overlay must never become part of the picture: a tile the gateway does not
// send again is a tile this page keeps showing. So the tiles are drawn onto a
// clean copy of the desktop held off screen, and the canvas the page shows is
// recomposed from it, a tile at a time, with the grid and the marks laid over:
// each tile's top and left edges in a faint line, which together are the grid,
// and a tile painted lately tinted, fading back to the picture.
//
// It runs in the paint worker, beside the painter that draws the tiles.

/** A tile's side: the gateway's grid, which is the host's (`TILE` in src/tiles.rs). */
export const TILE = 64;

/** How long a painted tile stays marked, fading. */
export const MARK_MS = 600;

/** How often the canvas is recomposed while a mark is fading. */
const FADE_STEP_MS = 50;

/** The grid's line, and the mark at its strongest. */
const GRID = "rgba(0, 200, 255, 0.35)";
const MARK = [255, 60, 60] as const;
const MARK_ALPHA = 0.45;

/** The part of a 2D context the overlay draws with. */
export interface OverlayContext {
  fillStyle: string | CanvasGradient | CanvasPattern;
  fillRect(x: number, y: number, w: number, h: number): void;
  drawImage(
    image: CanvasImageSource,
    sx: number,
    sy: number,
    sw: number,
    sh: number,
    dx: number,
    dy: number,
    dw: number,
    dh: number,
  ): void;
}

/** A canvas the clean picture is held on, and its context. */
export interface CleanCanvas {
  canvas: CanvasImageSource & { width: number; height: number };
  context: OverlayContext;
}

export interface TileDebug {
  /** The clean picture, where the painter draws the tiles. */
  context(): OverlayContext;
  /** The desktop is now `w`×`h` and black, on both canvases. */
  resize(w: number, h: number): void;
  /** These rectangles were painted onto the clean picture: mark them. */
  painted(
    rects: readonly { x: number; y: number; w: number; h: number }[],
  ): void;
  /** Recompose what changed onto the shown canvas. */
  present(): void;
  /** Stop: no more recomposing. */
  close(): void;
}

export function createTileDebug(options: {
  /** The canvas the page shows. */
  shown: OverlayContext;
  /** Make the canvas the clean picture is held on, at a size, black. */
  makeClean: (w: number, h: number) => CleanCanvas;
  w: number;
  h: number;
  now?: () => number;
  /** A timer, for the fade: injectable for a test. */
  later?: (run: () => void, ms: number) => void;
}): TileDebug {
  const now = options.now ?? (() => performance.now());
  const later =
    options.later ??
    ((run: () => void, ms: number) => {
      setTimeout(run, ms);
    });
  let w = options.w;
  let h = options.h;
  let clean = options.makeClean(w, h);
  let columns = Math.ceil(w / TILE);
  let rows = Math.ceil(h / TILE);
  // Tiles to recompose at the next present, and when each marked tile was painted.
  let stale = new Set<number>();
  let marks = new Map<number, number>();
  let closed = false;
  let fading = false;

  const everyTile = () => {
    stale = new Set(Array.from({ length: columns * rows }, (_, i) => i));
  };
  everyTile();

  const compose = (index: number, at: number) => {
    const col = index % columns;
    const row = Math.floor(index / columns);
    const x = col * TILE;
    const y = row * TILE;
    const tw = Math.min(TILE, w - x);
    const th = Math.min(TILE, h - y);
    const { shown } = options;
    shown.drawImage(clean.canvas, x, y, tw, th, x, y, tw, th);
    const painted = marks.get(index);
    if (painted !== undefined) {
      const left = 1 - (at - painted) / MARK_MS;
      if (left > 0) {
        shown.fillStyle = `rgba(${MARK[0]}, ${MARK[1]}, ${MARK[2]}, ${(MARK_ALPHA * left).toFixed(3)})`;
        shown.fillRect(x, y, tw, th);
      } else {
        marks.delete(index);
      }
    }
    shown.fillStyle = GRID;
    shown.fillRect(x, y, tw, 1);
    shown.fillRect(x, y, 1, th);
  };

  const present = () => {
    if (closed) {
      return;
    }
    const at = now();
    // A fading mark is recomposed until it is gone, and once more without it.
    for (const index of marks.keys()) {
      stale.add(index);
    }
    for (const index of stale) {
      compose(index, at);
    }
    stale.clear();
    if (marks.size > 0 && !fading) {
      fading = true;
      later(() => {
        fading = false;
        present();
      }, FADE_STEP_MS);
    }
  };

  return {
    context: () => clean.context,
    resize(nextW, nextH) {
      w = nextW;
      h = nextH;
      columns = Math.ceil(w / TILE);
      rows = Math.ceil(h / TILE);
      clean = options.makeClean(w, h);
      marks = new Map();
      everyTile();
      present();
    },
    painted(rects) {
      const at = now();
      for (const rect of rects) {
        // A rectangle may span cells, though the gateway's never do.
        const lastRow = Math.min(
          rows - 1,
          Math.floor((rect.y + rect.h - 1) / TILE),
        );
        const lastCol = Math.min(
          columns - 1,
          Math.floor((rect.x + rect.w - 1) / TILE),
        );
        for (let row = Math.floor(rect.y / TILE); row <= lastRow; row += 1) {
          for (let col = Math.floor(rect.x / TILE); col <= lastCol; col += 1) {
            const index = row * columns + col;
            marks.set(index, at);
            stale.add(index);
          }
        }
      }
    },
    present,
    close() {
      closed = true;
    },
  };
}
