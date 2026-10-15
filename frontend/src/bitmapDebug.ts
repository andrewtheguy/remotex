// TESTING ONLY: what a debug session of passed bitmap updates draws over the
// picture (`bitmapsStart` with `debug`, from a target's
// `bitmap_passthrough = "debug"`): the grid of 64-pixel tiles a Windows host sends
// its bitmap updates in, and each tile tinted as it arrives, fading after.
//
// The picture itself is left exact. What is kept here is when each tile last
// arrived; the picture draws the grid and the tint from it on the GPU
// (egfxPicture.ts), so the composed framebuffer is never written to.

/** The side of the tiles a Windows host sends its bitmap updates in. */
export const TILE = 64;

/** How long a tile stays tinted after it arrives, fading all the while. */
export const FADE_MS = 600;

export interface TileHeat {
  /** Tiles across the desktop, the last of them cut short where it ends. */
  readonly columns: number;
  /** Tiles down it. */
  readonly rows: number;
  /**
   * The tiles `painted` touches arrived `now`: `x, y, width, height` for each
   * rectangle, in the desktop's pixels.
   */
  stamp(painted: ArrayLike<number>, now: number): void;
  /**
   * How warm each tile is `now`, row by row from the top: 255 as it arrives, 0
   * once it has faded. `warm` is whether any still is, which is whether the
   * picture has to be drawn again for the fade.
   */
  sample(now: number): { heat: Uint8Array; warm: boolean };
}

/** The tiles of a desktop of `width` by `height`, none of them arrived yet. */
export function createTileHeat(width: number, height: number): TileHeat {
  const columns = Math.ceil(width / TILE);
  const rows = Math.ceil(height / TILE);
  const arrived = new Float64Array(columns * rows).fill(
    Number.NEGATIVE_INFINITY,
  );
  const heat = new Uint8Array(columns * rows);
  return {
    columns,
    rows,
    stamp(painted, now) {
      for (let i = 0; i + 3 < painted.length; i += 4) {
        const [x, y, w, h] = [
          painted[i],
          painted[i + 1],
          painted[i + 2],
          painted[i + 3],
        ];
        if (w === 0 || h === 0) {
          continue;
        }
        const right = Math.min(Math.floor((x + w - 1) / TILE), columns - 1);
        const bottom = Math.min(Math.floor((y + h - 1) / TILE), rows - 1);
        for (let row = Math.floor(y / TILE); row <= bottom; row += 1) {
          for (
            let column = Math.floor(x / TILE);
            column <= right;
            column += 1
          ) {
            arrived[row * columns + column] = now;
          }
        }
      }
    },
    sample(now) {
      let warm = false;
      for (let i = 0; i < arrived.length; i += 1) {
        const left = 1 - (now - arrived[i]) / FADE_MS;
        if (left > 0) {
          warm = true;
          heat[i] = Math.round(Math.min(left, 1) * 255);
        } else {
          heat[i] = 0;
        }
      }
      return { heat, warm };
    },
  };
}
