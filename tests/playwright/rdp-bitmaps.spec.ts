// TESTING ONLY: what a session that passes an RDP host's bitmap updates puts on
// the display socket, and what the page says it did with them. A target with
// `egfx = false` and `bitmap_passthrough` on passes them in every session, for the
// browser to compose with the pipeline's compositor and canvas.
//
// Everything asserted is decided by the system and not by a machine's timing: the
// render the gateway resolved, the order of `bitmapsStart` and the records behind
// it, the records' own framing, the acknowledgment the page sends once its paint
// worker has composed a batch, and the DOM's account of it. Nothing here looks at
// a pixel. A GRAPHICS record is parsed here for itself, as Bitmap Updates laid end
// to end ([MS-RDPBCGR] 2.2.9.1.1.3.1.2), and what is inside one is the host's.
//
// It needs a gateway whose local config names a live RDP host with the pipeline
// off and the updates passed. Keep that gitignored file under `tmp/`, for example
// `tmp/test_windows_arm_bitmaps.toml`:
//
//     cargo run -- serve --config tmp/test_windows_arm_bitmaps.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52892/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_BITMAPS_TARGET=windows-arm \
//     bunx playwright test '/rdp-bitmaps\.spec\.ts$'
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo, returnToPicker } from "./support";

/// The opt-in, and the target name in one.
const BITMAPS_TARGET = process.env.REMOTEX_PLAYWRIGHT_BITMAPS_TARGET;

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_GRAPHICS = 0x04;
const GRAPHICS_HEADER_LEN = 5;
/// `TS_UPDATE_BITMAP_DATA`: `updateType`, which is `UPDATETYPE_BITMAP`, and the
/// count of rectangles; then each `TS_BITMAP_DATA`, nine 16-bit fields and the
/// bitmap, whose length is the last of them.
const UPDATETYPE_BITMAP = 0x0001;
const UPDATE_HEADER_LEN = 4;
const RECTANGLE_HEADER_LEN = 18;

/// How many rectangles `run` holds, read as whole Bitmap Updates end to end, or
/// null where it is not.
function rectangles(run: Buffer): number | null {
  let count = 0;
  let at = 0;
  while (at < run.length) {
    if (at + UPDATE_HEADER_LEN > run.length) {
      return null;
    }
    if (run.readUInt16LE(at) !== UPDATETYPE_BITMAP) {
      return null;
    }
    const inUpdate = run.readUInt16LE(at + 2);
    at += UPDATE_HEADER_LEN;
    for (let i = 0; i < inUpdate; i += 1) {
      if (at + RECTANGLE_HEADER_LEN > run.length) {
        return null;
      }
      at += RECTANGLE_HEADER_LEN + run.readUInt16LE(at + 16);
    }
    count += inUpdate;
  }
  return at === run.length ? count : null;
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  /** Each record's rectangles, null for one that is not whole Bitmap Updates. */
  runs: (number | null)[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
}

function parseBatch(payload: Buffer): Batch {
  const runs: (number | null)[] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let badOp: number | undefined;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    if (op !== OP_GRAPHICS) {
      badOp = op;
      exact = false;
      break;
    }
    if (at + GRAPHICS_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const length = payload.readUInt32LE(at + 1);
    const start = at + GRAPHICS_HEADER_LEN;
    if (start + length > payload.length) {
      exact = false;
      break;
    }
    runs.push(rectangles(payload.subarray(start, start + length)));
    at = start + length;
  }
  return {
    flags: payload.readUInt8(1),
    count: payload.readUInt16LE(2),
    sequence: payload.readUInt32LE(4),
    runs,
    exact: exact && at === payload.length,
    badOp,
  };
}

interface Session {
  /** Every control message's `type`, in arrival order. */
  controlTypes: string[];
  connected?: { render: string };
  /** The last `resize` and `bitmapsStart`, as sent. */
  resize?: { w: number; h: number };
  start?: { w: number; h: number; debug: boolean };
  batches: Batch[];
  /** Binary frames that were not batches. */
  badKinds: number[];
  /** Runs that arrived on a display socket before its `bitmapsStart`. */
  unannounced: number;
  /** The sequences the page acknowledged, as it sent them. */
  acknowledged: number[];
}

/// Watch the session socket, and the display socket that carries its picture.
/// Registered before navigation, so nothing is missed.
function watchSession(page: Page): Session {
  const seen: Session = {
    controlTypes: [],
    batches: [],
    badKinds: [],
    unannounced: 0,
    acknowledged: [],
  };
  page.on("websocket", (ws) => {
    const url = new URL(ws.url());
    if (url.pathname === "/ws") {
      ws.on("framereceived", ({ payload }) => {
        if (typeof payload !== "string") {
          return;
        }
        const message = JSON.parse(payload);
        if (typeof message.type !== "string") {
          return;
        }
        seen.controlTypes.push(message.type);
        if (message.type === "connected") {
          seen.connected = { render: message.render };
        }
      });
      return;
    }
    if (url.pathname !== "/ws/display") {
      return;
    }
    // A display socket is an attachment of its own, whose batches and
    // acknowledgments are numbered from one, and which is told where the
    // updates start on its own.
    seen.batches = [];
    seen.acknowledged = [];
    let started = false;
    ws.on("framesent", ({ payload }) => {
      if (typeof payload !== "string") {
        return;
      }
      const message = JSON.parse(payload);
      if (message.type === "paintAck") {
        seen.acknowledged.push(message.sequence);
      }
    });
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        const message = JSON.parse(payload);
        if (typeof message.type !== "string") {
          return;
        }
        seen.controlTypes.push(message.type);
        if (message.type === "resize") {
          seen.resize = { w: message.w, h: message.h };
        }
        if (message.type === "bitmapsStart") {
          started = true;
          seen.start = { w: message.w, h: message.h, debug: message.debug };
        }
        return;
      }
      const kind = payload.readUInt8(0);
      if (kind !== BATCH_FRAME_KIND) {
        seen.badKinds.push(kind);
        return;
      }
      const batch = parseBatch(payload);
      if (!started) {
        seen.unannounced += batch.runs.length;
      }
      seen.batches.push(batch);
    });
  });
  return seen;
}

const runs = (seen: Session) => seen.batches.flatMap((batch) => batch.runs);

test.describe("a target that passes its bitmap updates", () => {
  test.skip(
    !BITMAPS_TARGET,
    "set REMOTEX_PLAYWRIGHT_BITMAPS_TARGET=<target> against a gateway with a live RDP host",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("is sent the host's bitmap updates, and composes every batch of them", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, BITMAPS_TARGET ?? "", "", {});

    await expect
      .poll(() => runs(seen).length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    // Composed, and not merely received: the acknowledgment is sent when the paint
    // worker has finished a batch.
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    expect(seen.connected?.render).toBe(
      "the host's bitmap updates, passed through",
    );
    expect(seen.badKinds, "binary frames that were not batches").toEqual([]);
    expect(
      seen.unannounced,
      "runs that arrived before their display socket's bitmapsStart",
    ).toBe(0);
    expect(
      seen.controlTypes,
      "passed bitmap updates are no video stream",
    ).not.toContain("videoFormat");
    expect(
      seen.controlTypes.indexOf("resize"),
      "the desktop's size is announced ahead of the updates that paint it",
    ).toBeLessThan(seen.controlTypes.indexOf("bitmapsStart"));
    expect(
      seen.start && { w: seen.start.w, h: seen.start.h },
      "the updates start on a desktop of the size announced",
    ).toEqual(seen.resize);

    for (const batch of seen.batches) {
      expect(batch.flags, "reserved frame flags must be zero").toBe(0);
      expect(batch.badOp, "every record must be a GRAPHICS record").toBe(
        undefined,
      );
      expect(batch.exact, "records must exactly fill the frame").toBe(true);
      expect(
        batch.runs.length,
        "the header's record count must match the records present",
      ).toBe(batch.count);
    }
    for (const count of runs(seen)) {
      expect(count, "a run is whole Bitmap Updates").not.toBeNull();
      expect(count).toBeGreaterThan(0);
    }
    // Acknowledged in the order they were sent, none twice and none invented.
    const sent = seen.batches.map((batch) => batch.sequence);
    expect(sent).toEqual(sent.map((_, index) => index + 1));
    expect(seen.acknowledged).toEqual(
      seen.acknowledged.map((_, index) => index + 1),
    );
    expect(Math.max(...seen.acknowledged)).toBeLessThanOrEqual(
      Math.max(...sent),
    );

    // A compositor that refused an update says so where the desktop is.
    await expect(page.getByRole("alert")).toHaveCount(0);
    // Drawn on the pipeline's own canvas, laid over the desktop's.
    await expect(page.locator("canvas.graphics")).toBeVisible();

    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Info", exact: true }).click();
    const card = page.getByRole("dialog", { name: "Info" });
    await expect(card).toContainText(
      "the host's bitmap updates are composed by this browser",
    );
    await expect(card).toContainText(
      "the host's bitmap updates, passed through",
    );
    await page.keyboard.press("Escape");
    await expect(card).toHaveCount(0);
  });

  test("starts the desktop over for a page that comes back, and is repainted", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, BITMAPS_TARGET ?? "", "", {});
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    // Nothing in an update is drawn against another, so the page that comes back
    // is told the updates start again, on a blank desktop the host repaints.
    const before = seen.controlTypes.length;
    await page.reload();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    await expect
      .poll(() => seen.controlTypes.slice(before), { timeout: 20_000 })
      .toContain("bitmapsStart");
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    expect(
      seen.unannounced,
      "runs that arrived before their display socket's bitmapsStart",
    ).toBe(0);
    for (const count of runs(seen)) {
      expect(count, "a run is whole Bitmap Updates").not.toBeNull();
    }
    await expect(page.getByRole("alert")).toHaveCount(0);
    await expect(page.locator("canvas.graphics")).toBeVisible();

    await returnToPicker(page);
  });
});
