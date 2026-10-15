// What a session started with Motion (alpha) puts on the display socket: an RDP
// host's graphics pipeline composed by the gateway, sent as PNG tiles where the
// desktop is still and as a VP9 stream of its own for each region that moves.
//
// Everything asserted is decided by the system and not by a machine's timing: the
// render the gateway resolved, `tilesStart` ahead of every record, the records' own
// framing, each tile's PNG header against the size its record states, each
// region's `regionFormat` ahead of its units and its keyframe first, no tile inside
// a live region, every pixel of the desktop drawn by some record, and the Info
// card's rows. Nothing here looks at a pixel: a TILE and a REGION are headers this
// file parses for itself, and a PNG is read no further than its IHDR.
//
// It needs a gateway whose local config names a live RDP host with the pipeline
// on and one virtual display, which offers Motion. Keep that gitignored file
// under `tmp/`, for example `tmp/test_windows_arm_motion.toml`:
//
//     cargo run -- serve --config tmp/test_windows_arm_motion.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52892/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_MOTION_TARGET=windows-arm \
//     bunx playwright test '/rdp-motion\.spec\.ts$'
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo, returnToPicker } from "./support";

/// The opt-in, and the target name in one, as the other live specs have it.
const MOTION_TARGET = process.env.REMOTEX_PLAYWRIGHT_MOTION_TARGET;

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_TILE = 0x05;
const OP_REGION = 0x06;
const TILE_HEADER_LEN = 13;
const REGION_HEADER_LEN = 15;
const REGION_KEYFRAME = 0x01;
/// A PNG's signature and its IHDR, which the PNG specification puts first.
const PNG_SIGNATURE = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
/// 8-bit truecolour, which every tile is.
const PNG_RGB = 2;

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/// One record, in wire order with the control messages around it.
type Item =
  | { kind: "tilesStart" }
  | { kind: "resize"; w: number; h: number }
  | { kind: "regionFormat"; stream: number }
  | { kind: "regionEnd"; stream: number }
  | { kind: "tile"; rect: Rect; png: boolean; sized: boolean; rgb: boolean }
  | {
      kind: "region";
      stream: number;
      rect: Rect;
      keyframe: boolean;
      flags: number;
    };

interface Display {
  /** Everything the display socket said of the picture, in order. */
  items: Item[];
  /** Binary frames that were not batches, or batches this parser rejected. */
  bad: string[];
  /** The batch sequences sent, and those the page acknowledged. */
  sent: number[];
  acknowledged: number[];
  /** Control messages that belong to another picture. */
  foreign: string[];
}

interface Session {
  connected?: { render: string };
  display: Display;
}

function emptyDisplay(): Display {
  return { items: [], bad: [], sent: [], acknowledged: [], foreign: [] };
}

/// Whether `png` is a PNG of `w`×`h`, and an 8-bit RGB one.
function readPng(
  png: Buffer,
  w: number,
  h: number,
): { png: boolean; sized: boolean; rgb: boolean } {
  const isPng =
    png.length >= 26 &&
    png.subarray(0, 8).equals(PNG_SIGNATURE) &&
    png.toString("latin1", 12, 16) === "IHDR";
  if (!isPng) {
    return { png: false, sized: false, rgb: false };
  }
  return {
    png: true,
    sized: png.readUInt32BE(16) === w && png.readUInt32BE(20) === h,
    rgb: png.readUInt8(24) === 8 && png.readUInt8(25) === PNG_RGB,
  };
}

function parseBatch(payload: Buffer, display: Display): void {
  if (payload.readUInt8(1) !== 0) {
    display.bad.push("nonzero frame flags");
  }
  const count = payload.readUInt16LE(2);
  display.sent.push(payload.readUInt32LE(4));
  let at = BATCH_HEADER_LEN;
  let records = 0;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    const header = op === OP_TILE ? TILE_HEADER_LEN : REGION_HEADER_LEN;
    if (op !== OP_TILE && op !== OP_REGION) {
      display.bad.push(`record op ${op}`);
      return;
    }
    if (at + header > payload.length) {
      display.bad.push("a truncated record header");
      return;
    }
    const base = op === OP_TILE ? at + 1 : at + 3;
    const rect = {
      x: payload.readUInt16LE(base),
      y: payload.readUInt16LE(base + 2),
      w: payload.readUInt16LE(base + 4),
      h: payload.readUInt16LE(base + 6),
    };
    const length = payload.readUInt32LE(base + 8);
    const start = at + header;
    if (length === 0 || start + length > payload.length) {
      display.bad.push("a record whose payload is empty or overruns its frame");
      return;
    }
    const body = payload.subarray(start, start + length);
    if (op === OP_TILE) {
      display.items.push({
        kind: "tile",
        rect,
        ...readPng(body, rect.w, rect.h),
      });
    } else {
      const flags = payload.readUInt8(at + 2);
      display.items.push({
        kind: "region",
        stream: payload.readUInt8(at + 1),
        rect,
        keyframe: (flags & REGION_KEYFRAME) !== 0,
        flags,
      });
    }
    records += 1;
    at = start + length;
  }
  if (records !== count) {
    display.bad.push(`a header counting ${count} records over ${records}`);
  }
}

/// Watch the session socket, and the display socket that carries the picture.
/// Registered before navigation, so nothing is missed.
function watchSession(page: Page): Session {
  const seen: Session = { display: emptyDisplay() };
  page.on("websocket", (ws) => {
    const url = new URL(ws.url());
    if (url.pathname === "/ws") {
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
    if (url.pathname !== "/ws/display") {
      return;
    }
    // A display socket is an attachment of its own, whose batches are numbered
    // from one and whose picture starts at its own `tilesStart`.
    const display = emptyDisplay();
    seen.display = display;
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (message.type === "paintAck") {
        display.acknowledged.push(message.sequence);
      }
    });
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string") {
        if (payload.readUInt8(0) !== BATCH_FRAME_KIND) {
          display.bad.push(`frame kind ${payload.readUInt8(0)}`);
          return;
        }
        parseBatch(payload, display);
        return;
      }
      const message = JSON.parse(payload);
      switch (message.type) {
        case "tilesStart":
          display.items.push({ kind: "tilesStart" });
          break;
        case "resize":
          display.items.push({ kind: "resize", w: message.w, h: message.h });
          break;
        case "regionFormat":
          display.items.push({ kind: "regionFormat", stream: message.stream });
          break;
        case "regionEnd":
          display.items.push({ kind: "regionEnd", stream: message.stream });
          break;
        case "graphicsStart":
        case "videoFormat":
          display.foreign.push(message.type);
          break;
      }
    });
  });
  return seen;
}

const overlaps = (a: Rect, b: Rect): boolean =>
  a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;

interface Walk {
  /** What broke the picture's rules, in words. */
  faults: string[];
  /** Pixels of the desktop no record has drawn since the last `tilesStart`. */
  undrawn: number;
  tiles: number;
}

/// Walk a display socket's picture in wire order, as the page draws it, and say
/// what broke its rules and how much of the desktop is still undrawn.
function walk(display: Display): Walk {
  const faults: string[] = [];
  let size: { w: number; h: number } | undefined;
  let started = false;
  let drawn: Uint8Array | undefined;
  /** Streams with a `regionFormat` not yet followed by a keyframe. */
  const formatted = new Set<number>();
  /** Each live stream's rectangle, from its first unit to its `regionEnd`. */
  const live = new Map<number, Rect>();
  let tiles = 0;
  const draw = (rect: Rect) => {
    if (!size || !drawn) {
      return;
    }
    if (rect.x + rect.w > size.w || rect.y + rect.h > size.h) {
      faults.push(`a record past the ${size.w}x${size.h} desktop`);
      return;
    }
    for (let y = rect.y; y < rect.y + rect.h; y += 1) {
      drawn.fill(1, y * size.w + rect.x, y * size.w + rect.x + rect.w);
    }
  };
  for (const item of display.items) {
    switch (item.kind) {
      case "resize":
        size = { w: item.w, h: item.h };
        break;
      case "tilesStart":
        started = true;
        drawn = size ? new Uint8Array(size.w * size.h) : undefined;
        formatted.clear();
        live.clear();
        break;
      case "regionFormat":
        formatted.add(item.stream);
        live.delete(item.stream);
        break;
      case "regionEnd":
        live.delete(item.stream);
        break;
      case "tile":
        tiles += 1;
        if (!started) {
          faults.push("a TILE before tilesStart");
        }
        if (!item.png || !item.sized || !item.rgb) {
          faults.push(
            `a TILE at ${item.rect.x},${item.rect.y} that is not an 8-bit RGB PNG of ${item.rect.w}x${item.rect.h}`,
          );
        }
        for (const [stream, rect] of live) {
          if (overlaps(item.rect, rect)) {
            faults.push(`a TILE inside live region ${stream}`);
          }
        }
        draw(item.rect);
        break;
      case "region":
        if (!started) {
          faults.push("a REGION before tilesStart");
        }
        if ((item.flags & ~REGION_KEYFRAME) !== 0) {
          faults.push(`a REGION with flags ${item.flags}`);
        }
        if (formatted.has(item.stream)) {
          if (!item.keyframe) {
            faults.push(`region ${item.stream} starting without a keyframe`);
          }
          formatted.delete(item.stream);
          live.set(item.stream, item.rect);
        } else if (!live.has(item.stream)) {
          faults.push(`a REGION on stream ${item.stream} with no regionFormat`);
        }
        draw(item.rect);
        break;
    }
  }
  const undrawn = drawn ? drawn.length - drawn.reduce((a, b) => a + b, 0) : -1;
  return { faults, undrawn, tiles };
}

test.describe("a target started with Motion", () => {
  test.skip(
    !MOTION_TARGET,
    "set REMOTEX_PLAYWRIGHT_MOTION_TARGET=<target> against a gateway with a live RDP host",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("is sent the composed desktop as PNG tiles and regions of VP9", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, MOTION_TARGET ?? "", "", { motion: true });

    // The whole desktop is drawn by the opening's records, each one acknowledged
    // once the paint worker has drawn its batch.
    await expect
      .poll(() => walk(seen.display).undrawn, { timeout: 30_000 })
      .toBe(0);
    await expect
      .poll(() => seen.display.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    expect(seen.connected?.render).toMatch(/^motion as video q\d+ · libvpx/);
    const { faults, tiles } = walk(seen.display);
    expect(faults).toEqual([]);
    expect(tiles).toBeGreaterThan(0);
    expect(seen.display.bad, "frames this parser rejected").toEqual([]);
    expect(
      seen.display.foreign,
      "a composed picture is no passed pipeline and no desktop stream",
    ).toEqual([]);
    const sent = seen.display.sent;
    expect(sent).toEqual(sent.map((_, index) => index + 1));
    expect(seen.display.acknowledged).toEqual(
      seen.display.acknowledged.map((_, index) => index + 1),
    );
    await expect(page.getByRole("alert")).toHaveCount(0);

    // The card says how the picture is sent, and whether the host draws it
    // losslessly, which is the host's policy and so either answer here.
    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Info", exact: true }).click();
    const card = page.getByRole("dialog", { name: "Info" });
    await expect(card).toContainText("encoded by the gateway");
    await expect(card).toContainText("Host's picture");
    await expect(card).toContainText(/Lossless \(|Lossy \(/);
    await page.keyboard.press("Escape");
    await expect(card).toHaveCount(0);
    // The drawer stays open behind the card, its toggle reading "Close menu".
    await page.getByRole("button", { name: "Close menu" }).click();

    await returnToPicker(page);
  });

  test("sends a page that comes back every tile again", async ({ page }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, MOTION_TARGET ?? "", "", { motion: true });
    await expect
      .poll(() => walk(seen.display).undrawn, { timeout: 30_000 })
      .toBe(0);

    // The gateway holds the picture, so the page that comes back is sent it
    // whole on its new display socket, behind a `tilesStart` of its own.
    const before = seen.display;
    await page.reload();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    await expect.poll(() => seen.display !== before).toBe(true);
    await expect
      .poll(() => walk(seen.display).undrawn, { timeout: 30_000 })
      .toBe(0);
    expect(walk(seen.display).faults).toEqual([]);
    expect(seen.display.bad).toEqual([]);
    expect(seen.display.sent[0]).toBe(1);
    await expect(page.getByRole("alert")).toHaveCount(0);

    await returnToPicker(page);
  });
});
