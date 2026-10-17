// What a target whose picture the gateway codes puts on the display socket: the
// desktop in 64x64 tiles, each a JPEG or a PNG, behind the `tilesStart` that begins
// the picture. Everything here is decidable without asking the browser anything:
// nothing looks at a pixel, and a TILE record is a header this file parses for
// itself.
//
// It needs a gateway whose local config hard-codes a plain `vnc` target or an `rdp`
// one with `egfx = false`. Keep that gitignored file under `tmp/`, for example
// `tmp/qa_tiles.toml`:
//
//     cargo run -- serve --config tmp/qa_tiles.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52889/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_TILES_TARGET=bitmap \
//     npx playwright test tiles
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo } from "./support";

/// The opt-in, and the target name in one — the same bargain the audio spec makes.
/// Its presence is the claim that this gateway has a live target whose picture it
/// codes; without one the spec would be asserting against a picture that never
/// arrives.
const TILES_TARGET = process.env.REMOTEX_PLAYWRIGHT_TILES_TARGET;

/// The wire, copied from src/protocol.rs rather than imported from the SPA: this spec
/// is the independent check that the gateway and the client agree, and reading the
/// client's own parser to decide that would be asking the accused.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_TILE = 0x06;
const TILE_HEADER_LEN = 14;
const TILE_JPEG = 0x01;
const TILE_PNG = 0x02;
const TILE = 64;

interface TileRecord {
  format: number;
  x: number;
  y: number;
  w: number;
  h: number;
  /** Whether the image starts as its format says: a JPEG's SOI, a PNG's signature. */
  magic: boolean;
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  tiles: TileRecord[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
}

function parseBatch(payload: Buffer): Batch {
  const count = payload.readUInt16LE(2);
  const tiles: TileRecord[] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let badOp: number | undefined;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    if (op !== OP_TILE) {
      // An op this parser does not know stops it here, which is where the bad byte
      // is. Reading on would take a length out of somebody else's bytes and fail
      // further along, looking like truncation instead.
      badOp = op;
      exact = false;
      break;
    }
    if (at + TILE_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const format = payload.readUInt8(at + 1);
    const len = payload.readUInt32LE(at + 10);
    const image = payload.subarray(
      at + TILE_HEADER_LEN,
      at + TILE_HEADER_LEN + len,
    );
    const magic =
      format === TILE_JPEG
        ? image[0] === 0xff && image[1] === 0xd8
        : image.subarray(0, 4).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47]));
    tiles.push({
      format,
      x: payload.readUInt16LE(at + 2),
      y: payload.readUInt16LE(at + 4),
      w: payload.readUInt16LE(at + 6),
      h: payload.readUInt16LE(at + 8),
      magic,
    });
    at += TILE_HEADER_LEN + len;
  }
  return {
    flags: payload.readUInt8(1),
    count,
    sequence: payload.readUInt32LE(4),
    tiles,
    exact: exact && at === payload.length,
    badOp,
  };
}

interface Session {
  connected?: { render: string };
  /** `tilesStart`s seen, which every tile must follow. */
  starts: number;
  /** `videoFormat`s seen, of which a picture coded here has none. */
  formats: number;
  /** The last `resize`, which every tile after it must lie inside. */
  resize?: { w: number; h: number };
  batches: Batch[];
  /** Binary frames that were not batches — audio has a socket of its own. */
  badKinds: number[];
  /** Tiles that arrived before any `tilesStart`. */
  unstarted: number;
  /** Tiles off the 64-pixel grid or past the desktop the last `resize` announced. */
  misplaced: number;
  /** The grid's tiles sent since the last resize, as `col,row`. */
  covered: Set<string>;
  /** Batches the page acknowledged, by sequence. */
  acked: number[];
}

/// A control message, as far as this spec reads it.
function onControl(seen: Session, message: Record<string, unknown>): void {
  switch (message.type) {
    case "connected":
      seen.connected = { render: String(message.render) };
      break;
    case "tilesStart":
      seen.starts += 1;
      break;
    case "videoFormat":
      seen.formats += 1;
      break;
    case "resize":
      seen.resize = { w: Number(message.w), h: Number(message.h) };
      seen.covered.clear();
      break;
  }
}

/// Whether `tile` lies on the 64-pixel grid inside the desktop `resize` names.
function inside(tile: TileRecord, resize: Session["resize"]): boolean {
  return (
    resize !== undefined &&
    tile.x % TILE === 0 &&
    tile.y % TILE === 0 &&
    tile.w > 0 &&
    tile.h > 0 &&
    tile.x + tile.w <= resize.w &&
    tile.y + tile.h <= resize.h
  );
}

/// One tile record: counted against its start and its desktop, and its grid tiles
/// marked sent.
function onTile(seen: Session, tile: TileRecord): void {
  if (seen.starts === 0) {
    seen.unstarted += 1;
  }
  if (!inside(tile, seen.resize)) {
    seen.misplaced += 1;
    return;
  }
  const cols = Math.ceil((tile.x + tile.w) / TILE);
  const rows = Math.ceil((tile.y + tile.h) / TILE);
  for (let col = tile.x / TILE; col < cols; col += 1) {
    for (let row = tile.y / TILE; row < rows; row += 1) {
      seen.covered.add(`${col},${row}`);
    }
  }
}

/// Watch the session and display sockets. Registered before navigation, so nothing
/// is missed.
function watchSession(page: Page): Session {
  const seen: Session = {
    starts: 0,
    formats: 0,
    batches: [],
    badKinds: [],
    unstarted: 0,
    misplaced: 0,
    covered: new Set(),
    acked: [],
  };
  page.on("websocket", (ws) => {
    const path = new URL(ws.url()).pathname;
    if (path !== "/ws" && path !== "/ws/display") {
      return;
    }
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (message.type === "paintAck") {
        seen.acked.push(message.sequence);
      }
    });
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        onControl(seen, JSON.parse(payload));
        return;
      }
      const kind = payload.readUInt8(0);
      if (kind !== BATCH_FRAME_KIND) {
        seen.badKinds.push(kind);
        return;
      }
      const batch = parseBatch(payload);
      for (const tile of batch.tiles) {
        onTile(seen, tile);
      }
      seen.batches.push(batch);
    });
  });
  return seen;
}

const tiles = (seen: Session): TileRecord[] =>
  seen.batches.flatMap((b) => b.tiles);

/// Whether every tile of the desktop the last resize announced has been sent.
const whole = (seen: Session): boolean =>
  seen.resize !== undefined &&
  seen.covered.size ===
    Math.ceil(seen.resize.w / TILE) * Math.ceil(seen.resize.h / TILE);

/// Every assertion that holds for the picture.
function assertTheEnvelopeHolds(seen: Session): void {
  expect(seen.badKinds, "binary frames that were not batches").toEqual([]);
  expect(seen.unstarted, "tiles that arrived before any tilesStart").toBe(0);
  expect(
    seen.misplaced,
    "tiles off the grid or past the desktop the last resize announced",
  ).toBe(0);
  expect(seen.formats, "a stream announced for a picture coded here").toBe(0);

  for (const batch of seen.batches) {
    expect(batch.flags, "reserved frame flags must be zero").toBe(0);
    expect(batch.badOp, "every record op must be one this build knows").toBe(
      undefined,
    );
    expect(batch.exact, "records must exactly fill the frame").toBe(true);
    expect(
      batch.tiles.length,
      "the header's record count must match the records present",
    ).toBe(batch.count);
  }
  expect(
    seen.batches.map((batch) => batch.sequence),
    "screen batch sequences must increase in socket order",
  ).toEqual(seen.batches.map((_, index) => index + 1));

  for (const tile of tiles(seen)) {
    expect([TILE_JPEG, TILE_PNG], "a tile format this build knows").toContain(
      tile.format,
    );
    expect(tile.magic, "a tile whose image is not its format").toBe(true);
  }
}

test.describe("a target whose picture the gateway codes", () => {
  test.skip(
    !TILES_TARGET,
    "set REMOTEX_PLAYWRIGHT_TILES_TARGET=<target> against a gateway with a live plain vnc or egfx = false target",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("sends the whole desktop as tiles behind their start, and the page acknowledges them", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, TILES_TARGET ?? "");

    await expect.poll(() => whole(seen), { timeout: 20_000 }).toBe(true);
    // The page draws each batch and says so: the gateway sends the next tiles as
    // it paints.
    await expect
      .poll(() => seen.acked.length, { timeout: 10_000 })
      .toBeGreaterThan(0);

    expect(seen.connected?.render).toMatch(/^tiles q\d+$/);
    expect(seen.starts).toBeGreaterThan(0);
    assertTheEnvelopeHolds(seen);
  });
});
