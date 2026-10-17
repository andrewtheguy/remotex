// What a session started on a Standard Mac's Combined Display puts on the display
// socket: every screen at once, asked of the Mac in its Full quality and composed
// and coded by the gateway as its 64x64 tiles. Screen Sharing Standard's alone.
//
// Over a 1x screen beside a 2x one, a 1x browser is sent the screens composed at
// its density, the 2x one scaled down, as one plain desktop with no mosaic; a 2x
// browser is sent the Mac's own pixels with the mosaic it composes them by. Either
// way no display list is sent, since there is no single screen to choose, and the
// picture ceiling counts the pixels each screen is sent at, which a 1x and a 2x
// screen of a laptop's size are well within.
//
// Everything asserted is the gateway's decision: the render it resolved, the
// control messages, the TILE records' framing and place, and the page's
// acknowledgments. Nothing looks at a pixel.
//
// It needs a gateway whose local config names an `ard` target on a Mac with a 1x
// screen beside a 2x one. Keep that gitignored file under `tmp/`, for example
// `tmp/test_uat.toml`:
//
//     cargo run -- serve --config tmp/test_uat.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52888/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_STANDARD_COMBINED_TARGET=macvmstandard \
//     bunx playwright test '/standard-combined\.spec\.ts$'
import { expect, type Page, test } from "@playwright/test";

import { faults, leaveSession, logInAndConnectTo, returnToPicker } from "./support";

const COMBINED_TARGET = process.env.REMOTEX_PLAYWRIGHT_STANDARD_COMBINED_TARGET;

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_TILE = 0x06;
const TILE_HEADER_LEN = 14;
const TILE = 64;
/// The ceiling's area, which Combined Display is counted against
/// (`video::MAX_PIXELS`).
const MAX_PIXELS = 3840 * 2400;

interface Region {
  pixels: { x: number; y: number; w: number; h: number };
  points: { x: number; y: number; w: number; h: number };
}

interface Session {
  connected?: { render: string; passthrough: string | null };
  /** Every control message's `type`, in arrival order. */
  controlTypes: string[];
  /** The last `resize`. */
  resize?: { w: number; h: number; scale: number };
  /** Every `mosaic`'s regions, in arrival order. */
  mosaics: Region[][];
  /** The last `oversize`'s cause, and every one in arrival order. */
  oversize?: string | null;
  oversizes: (string | null)[];
  /** TILE records off the grid or past the last `resize`. */
  misplaced: number;
  /** Records that were not tiles, and frames that were not batches. */
  strays: number;
  /** Batches whose records did not exactly fill them. */
  inexact: number;
  /** The grid's tiles sent since the last resize, as `col,row`. */
  covered: Set<string>;
  /** Batches the page acknowledged. */
  acked: number;
}

/// Read one batch's tiles, counting what does not hold.
function onBatch(seen: Session, payload: Buffer): void {
  let at = BATCH_HEADER_LEN;
  while (at < payload.length) {
    if (payload.readUInt8(at) !== OP_TILE || at + TILE_HEADER_LEN > payload.length) {
      seen.strays += 1;
      return;
    }
    const [x, y, w, h] = [2, 4, 6, 8].map((o) => payload.readUInt16LE(at + o));
    const size = seen.resize;
    if (!size || x % TILE !== 0 || y % TILE !== 0 || x + w > size.w || y + h > size.h) {
      seen.misplaced += 1;
    } else {
      for (let col = x / TILE; col < Math.ceil((x + w) / TILE); col += 1) {
        for (let row = y / TILE; row < Math.ceil((y + h) / TILE); row += 1) {
          seen.covered.add(`${col},${row}`);
        }
      }
    }
    at += TILE_HEADER_LEN + payload.readUInt32LE(at + 10);
  }
  if (at !== payload.length) {
    seen.inexact += 1;
  }
}

/// Watch the session and display sockets, registered before navigation.
function watchSession(page: Page): Session {
  const seen: Session = {
    controlTypes: [],
    mosaics: [],
    oversizes: [],
    misplaced: 0,
    strays: 0,
    inexact: 0,
    covered: new Set(),
    acked: 0,
  };
  page.on("websocket", (ws) => {
    const path = new URL(ws.url()).pathname;
    if (path !== "/ws" && path !== "/ws/display") {
      return;
    }
    ws.on("framesent", ({ payload }) => {
      if (typeof payload === "string" && JSON.parse(payload).type === "paintAck") {
        seen.acked += 1;
      }
    });
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        if (payload.readUInt8(0) === BATCH_FRAME_KIND) {
          onBatch(seen, payload);
        } else {
          seen.strays += 1;
        }
        return;
      }
      const message = JSON.parse(payload);
      if (typeof message.type !== "string") {
        return;
      }
      seen.controlTypes.push(message.type);
      if (message.type === "connected") {
        seen.connected = { render: message.render, passthrough: message.passthrough };
      } else if (message.type === "resize") {
        seen.resize = { w: message.w, h: message.h, scale: message.scale };
        seen.covered.clear();
      } else if (message.type === "mosaic") {
        seen.mosaics.push(message.regions);
      } else if (message.type === "oversize") {
        seen.oversize = message.cause;
        seen.oversizes.push(message.cause);
      }
    });
  });
  return seen;
}

/// What holds of a Combined Display session at either density.
async function assertTheSessionHolds(page: Page, seen: Session): Promise<void> {
  await expect
    .poll(() => seen.acked, { message: "batches of tiles painted", timeout: 20_000 })
    .toBeGreaterThan(0);
  expect(seen.connected?.render).toMatch(/^Combined Display, tiles q\d+$/);
  expect(seen.connected?.passthrough).toBe(null);
  expect(seen.controlTypes).toContain("tilesStart");
  expect(seen.controlTypes).not.toContain("standardStart");
  expect(seen.controlTypes, "no screen to choose").not.toContain("displays");
  // Never held on the way to the view its layout shows.
  expect(seen.oversizes, "held at no point").not.toContain("size");
  expect(seen.misplaced, "tiles off the grid or the desktop").toBe(0);
  expect(seen.strays, "records that were not tiles").toBe(0);
  expect(seen.inexact, "batches their records did not fill").toBe(0);
  await expect(faults(page)).toHaveCount(0);
}

test.describe("a Standard Mac's Combined Display, composed by the gateway", () => {
  test.skip(
    !COMBINED_TARGET,
    "set REMOTEX_PLAYWRIGHT_STANDARD_COMBINED_TARGET=<target> against a gateway with a live Mac of a 1x and a 2x screen",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("is one plain desktop at a 1x browser's density, the 2x screen scaled down", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    const seen = watchSession(page);
    await logInAndConnectTo(page, COMBINED_TARGET ?? "", "", { standardView: "combined" });
    await expect.poll(() => seen.resize?.scale, { timeout: 20_000 }).toBe(1);
    await assertTheSessionHolds(page, seen);
    expect(seen.mosaics.every((regions) => regions.length === 0), "no mosaic").toBe(true);
    await expect.poll(() => seen.oversize).toBe(null);
    // The gaps between unequal screens are sent too: the whole desktop is tiles.
    const size = seen.resize ?? { w: 0, h: 0 };
    await expect
      .poll(() => seen.covered.size, { message: "every tile of the desktop", timeout: 30_000 })
      .toBe(Math.ceil(size.w / TILE) * Math.ceil(size.h / TILE));
    await returnToPicker(page);
  });

  test.describe("at a 2x browser", () => {
    test.use({ deviceScaleFactor: 2 });

    test("is the Mac's pixels with the mosaic the page composes them by", async ({ page }) => {
      test.setTimeout(120_000);
      const seen = watchSession(page);
      await logInAndConnectTo(page, COMBINED_TARGET ?? "", "", { standardView: "combined" });
      await expect
        .poll(() => seen.mosaics.at(-1)?.length ?? 0, { timeout: 20_000 })
        .toBe(2);
      await assertTheSessionHolds(page, seen);
      const regions = seen.mosaics.at(-1) ?? [];
      const right = Math.max(...regions.map((r) => r.pixels.x + r.pixels.w));
      const bottom = Math.max(...regions.map((r) => r.pixels.y + r.pixels.h));
      expect(seen.resize).toMatchObject({ w: right, h: bottom, scale: 2 });
      // Counted by the pixels each screen is sent at, not the bounds they span.
      const pixels = regions.reduce((sum, r) => sum + r.pixels.w * r.pixels.h, 0);
      await expect.poll(() => seen.oversize).toBe(pixels > MAX_PIXELS ? "size" : null);
      await returnToPicker(page);
    });
  });
});
