// TESTING ONLY: what an RDP target with `egfx = false` and `png_tiles` puts on
// the display socket — a PNG of each 64×64 tile of the desktop that changed,
// behind a `tilesStart` that begins every picture the page starts from. All of
// it is decidable from the frames: which records come, where each tile lies on
// the grid, that each is a PNG of its own size, that the first picture after a
// `tilesStart` covers every cell, and that the page acknowledges each batch
// once its paint worker has drawn it. Nothing here looks at a pixel, the debug
// overlay's included.
//
// It needs a gateway whose local config has such a target, for example in
// `tmp/test_windows_arm_tiles.toml`:
//
//     [[targets]]
//     name = "windows-arm-tiles"
//     protocol = "rdp"
//     egfx = false
//     png_tiles = "debug"
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52892/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_TILES_TARGET=windows-arm-tiles \
//     bunx playwright test '/png-tiles\.spec\.ts$'
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo, returnToPicker } from "./support";

/// The opt-in, and the target name in one.
const TILES_TARGET = process.env.REMOTEX_PLAYWRIGHT_TILES_TARGET;

/// The wire, copied from src/protocol.rs rather than imported from the SPA: this
/// spec is the independent check that the gateway and the client agree.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_TILE = 0x05;
const TILE_HEADER_LEN = 13;
/// The grid, `TILE` in src/tiles.rs: the host's own.
const TILE = 64;
/// Every PNG opens with this, and its IHDR chunk's width and height follow at 16.
const PNG_SIGNATURE = Buffer.from([
  0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
]);

interface Tile {
  x: number;
  y: number;
  w: number;
  h: number;
  /** The size the PNG's own header gives, or null where it is no PNG. */
  png: { w: number; h: number } | null;
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  tiles: Tile[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op that was not a TILE, if any. */
  badOp?: number;
}

function parsePng(png: Buffer): Tile["png"] {
  if (png.length < 24 || !png.subarray(0, 8).equals(PNG_SIGNATURE)) {
    return null;
  }
  if (png.toString("latin1", 12, 16) !== "IHDR") {
    return null;
  }
  return { w: png.readUInt32BE(16), h: png.readUInt32BE(20) };
}

function parseBatch(payload: Buffer): Batch {
  const tiles: Tile[] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let badOp: number | undefined;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    if (op !== OP_TILE) {
      badOp = op;
      exact = false;
      break;
    }
    if (at + TILE_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const len = payload.readUInt32LE(at + 9);
    const start = at + TILE_HEADER_LEN;
    tiles.push({
      x: payload.readUInt16LE(at + 1),
      y: payload.readUInt16LE(at + 3),
      w: payload.readUInt16LE(at + 5),
      h: payload.readUInt16LE(at + 7),
      png: parsePng(
        payload.subarray(start, Math.min(start + len, payload.length)),
      ),
    });
    at = start + len;
  }
  return {
    flags: payload.readUInt8(1),
    count: payload.readUInt16LE(2),
    sequence: payload.readUInt32LE(4),
    tiles,
    exact: exact && at === payload.length,
    badOp,
  };
}

/// One picture the page starts from: a `tilesStart`, the desktop it is of, and
/// the cells the tiles after it have covered.
interface Picture {
  debug: boolean;
  w: number;
  h: number;
  covered: Set<string>;
}

interface Session {
  connected?: { render: string };
  /** Control messages on the display socket, in arrival order. */
  controlTypes: string[];
  batches: Batch[];
  pictures: Picture[];
  /** Binary frames that were not batches. */
  badKinds: number[];
  /** Tiles that arrived before any `tilesStart` on their socket. */
  unannounced: number;
  /** Tiles that are not exactly one cell of the grid of the desktop announced. */
  offGrid: Tile[];
  /** The sequences the page acknowledged, as it sent them. */
  acknowledged: number[];
}

/// Whether `tile` is exactly one cell of `picture`'s grid: on it, and as large as
/// the cell is inside the desktop, clipped at its right and bottom edges and
/// nowhere else.
function oneCell(tile: Tile, picture: Picture): boolean {
  return (
    tile.x % TILE === 0 &&
    tile.y % TILE === 0 &&
    tile.x < picture.w &&
    tile.y < picture.h &&
    tile.w === Math.min(TILE, picture.w - tile.x) &&
    tile.h === Math.min(TILE, picture.h - tile.y)
  );
}

/// Watch the session socket and the display socket that carries the picture.
/// Registered before navigation, so nothing is missed.
function watchSession(page: Page): Session {
  const seen: Session = {
    controlTypes: [],
    batches: [],
    pictures: [],
    badKinds: [],
    unannounced: 0,
    offGrid: [],
    acknowledged: [],
  };
  page.on("websocket", (ws) => {
    const path = new URL(ws.url()).pathname;
    if (path === "/ws") {
      ws.on("framereceived", ({ payload }) => {
        if (typeof payload !== "string") {
          return;
        }
        const message = JSON.parse(payload);
        if (message.type === "connected") {
          seen.connected = { render: message.render };
        }
      });
      return;
    }
    if (path !== "/ws/display") {
      return;
    }
    // A display socket is an attachment of its own, numbered from one, whose
    // picture starts with a `tilesStart` of its own.
    seen.batches = [];
    seen.acknowledged = [];
    let size: { w: number; h: number } | null = null;
    let picture: Picture | null = null;
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (message.type === "paintAck") {
        seen.acknowledged.push(message.sequence);
      }
    });
    const control = (text: string) => {
      const message = JSON.parse(text);
      if (typeof message.type !== "string") {
        return;
      }
      seen.controlTypes.push(message.type);
      if (message.type === "resize") {
        size = { w: message.w, h: message.h };
      }
      if (message.type === "tilesStart" && size) {
        picture = { debug: message.debug, ...size, covered: new Set() };
        seen.pictures.push(picture);
      }
    };
    // Each tile into the picture its socket last started.
    const place = (placed: Tile[]) => {
      for (const tile of placed) {
        if (!picture) {
          seen.unannounced += 1;
          continue;
        }
        if (!oneCell(tile, picture)) {
          seen.offGrid.push(tile);
        }
        picture.covered.add(`${tile.x},${tile.y}`);
      }
    };
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        control(payload);
        return;
      }
      const kind = payload.readUInt8(0);
      if (kind !== BATCH_FRAME_KIND) {
        seen.badKinds.push(kind);
        return;
      }
      const batch = parseBatch(payload);
      place(batch.tiles);
      seen.batches.push(batch);
    });
  });
  return seen;
}

const tiles = (seen: Session): Tile[] => seen.batches.flatMap((b) => b.tiles);

/// How many cells a picture's desktop has.
const cells = (picture: Picture): number =>
  Math.ceil(picture.w / TILE) * Math.ceil(picture.h / TILE);

/// The latest picture has had every cell of its desktop, and the page has drawn
/// what carried them.
async function expectWholePicture(seen: Session): Promise<Picture> {
  await expect
    .poll(
      () => {
        const picture = seen.pictures.at(-1);
        return picture ? cells(picture) - picture.covered.size : null;
      },
      { timeout: 20_000, message: "cells of the desktop no tile has covered" },
    )
    .toBe(0);
  const sent = seen.batches.length;
  await expect
    .poll(() => Math.max(0, ...seen.acknowledged), { timeout: 20_000 })
    .toBeGreaterThanOrEqual(sent);
  const picture = seen.pictures.at(-1);
  if (!picture) {
    throw new Error("no tilesStart was seen");
  }
  return picture;
}

/// Every assertion that holds for the tiles, whatever the desktop did.
function assertTheTilesHold(seen: Session): void {
  expect(seen.badKinds, "binary frames that were not batches").toEqual([]);
  expect(seen.unannounced, "tiles that arrived before a tilesStart").toBe(0);
  expect(seen.offGrid, "tiles that are not one cell of the grid").toEqual([]);
  expect(
    seen.controlTypes,
    "a picture sent as tiles is no video stream",
  ).not.toContain("videoFormat");
  expect(seen.controlTypes, "nor a pipeline").not.toContain("graphicsStart");
  expect(
    seen.controlTypes.indexOf("resize"),
    "the desktop's size is announced ahead of the tiles that draw it",
  ).toBeLessThan(seen.controlTypes.indexOf("tilesStart"));

  for (const batch of seen.batches) {
    expect(batch.flags, "reserved frame flags must be zero").toBe(0);
    expect(batch.badOp, "every record must be a TILE record").toBe(undefined);
    expect(batch.exact, "records must exactly fill the frame").toBe(true);
    expect(
      batch.tiles.length,
      "the header's record count must match the records present",
    ).toBe(batch.count);
  }
  for (const tile of tiles(seen)) {
    expect(tile.png, "a tile is a PNG of its own size").toEqual({
      w: tile.w,
      h: tile.h,
    });
  }
  const sent = seen.batches.map((batch) => batch.sequence);
  expect(sent).toEqual(sent.map((_, index) => index + 1));
  expect(seen.acknowledged).toEqual(
    seen.acknowledged.map((_, index) => index + 1),
  );
}

test.describe("a target that sends its picture as PNG tiles", () => {
  test.skip(
    !TILES_TARGET,
    "set REMOTEX_PLAYWRIGHT_TILES_TARGET=<target> against a gateway with an rdp target with egfx = false and png_tiles",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("is sent every cell of the desktop, then the cells that change, and draws them", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, TILES_TARGET ?? "");

    const picture = await expectWholePicture(seen);
    assertTheTilesHold(seen);

    // The card's line and the start say the same about the overlay.
    const overlay = picture.debug ? ", debug overlay" : "";
    expect(seen.connected?.render).toBe(`PNG tiles${overlay} (testing only)`);
    await expect(page.getByRole("alert")).toHaveCount(0);
    // Drawn on the desktop's own canvas: the one laid over it is the GPU's.
    await expect(page.locator("canvas.graphics")).toBeHidden();

    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Info", exact: true }).click();
    const card = page.getByRole("dialog", { name: "Info" });
    await expect(card).toContainText(
      `PNG tiles from the gateway${overlay} (testing only)`,
    );
    await page.keyboard.press("Escape");
    await expect(card).toHaveCount(0);
  });

  test("starts the picture over, every cell, for a page that comes back", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, TILES_TARGET ?? "");
    await expectWholePicture(seen);

    // The page that comes back holds nothing of the desktop, so it is sent all
    // of it again, behind a start of its own, however little has changed.
    const before = seen.pictures.length;
    await page.reload();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    await expect.poll(() => seen.pictures.length).toBeGreaterThan(before);
    await expectWholePicture(seen);
    assertTheTilesHold(seen);
    expect(seen.batches[0]?.sequence).toBe(1);
    await expect(page.getByRole("alert")).toHaveCount(0);

    await returnToPicker(page);
  });
});
