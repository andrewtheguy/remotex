// What a session started with a Standard Mac's Adaptive picture puts on the
// display socket, and what the page says it did with it: the Mac's own 8×8
// tiles, passed for the page to decode.
//
// Everything asserted is decided by the system and not by a machine's timing: the
// render the gateway resolved, the order of `adaptiveStart` and the records
// behind it, the records' own framing, that the first is the Mac's quantization
// tables, the acknowledgment the page sends once its paint worker has decoded a
// batch, and the DOM's account of a rectangle that did not decode. Nothing here
// looks at a pixel. An ADAPTIVE record is a header this file parses for itself
// around a body as the Mac sent it, whose first byte says what it is.
//
// It needs a gateway whose local config names a live Mac as an `ard` target. Keep
// that gitignored file under `tmp/`, for example `tmp/test_uat.toml`:
//
//     cargo run -- serve --config tmp/test_uat.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52888/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_ADAPTIVE_TARGET=macvmstandard \
//     bunx playwright test '/adaptive-passthrough\.spec\.ts$'
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo, returnToPicker } from "./support";

/// The opt-in, and the target name in one, as the other live specs have it.
const ADAPTIVE_TARGET = process.env.REMOTEX_PLAYWRIGHT_ADAPTIVE_TARGET;

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_ADAPTIVE = 0x05;
const ADAPTIVE_HEADER_LEN = 13;
/// What a body's first byte says it is (docs/apple-vnc-889.md).
const KINDS = ["full", "partial", "tables"] as const;
/// The quantization tables are exactly this long.
const TABLES_LEN = 129;

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
  kind: string;
  length: number;
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  rects: Rect[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
}

function parseBatch(payload: Buffer): Batch {
  const rects: Rect[] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let badOp: number | undefined;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    if (op !== OP_ADAPTIVE) {
      badOp = op;
      exact = false;
      break;
    }
    if (at + ADAPTIVE_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const field = (i: number) => payload.readUInt16LE(at + 1 + 2 * i);
    const length = payload.readUInt32LE(at + 9);
    const start = at + ADAPTIVE_HEADER_LEN;
    if (length === 0 || start + length > payload.length) {
      exact = false;
      break;
    }
    rects.push({
      x: field(0),
      y: field(1),
      w: field(2),
      h: field(3),
      kind: KINDS[payload.readUInt8(start)] ?? "unknown",
      length,
    });
    at = start + length;
  }
  return {
    flags: payload.readUInt8(1),
    count: payload.readUInt16LE(2),
    sequence: payload.readUInt32LE(4),
    rects,
    exact: exact && at === payload.length,
    badOp,
  };
}

interface Session {
  /** Every control message's `type`, in arrival order. */
  controlTypes: string[];
  connected?: { render: string; passthrough: string | null };
  /** The last size the display socket announced, in pixels. */
  size?: { w: number; h: number };
  batches: Batch[];
  /** Binary frames that were not batches. */
  badKinds: number[];
  /** Rectangles that arrived before their socket's `adaptiveStart`. */
  unannounced: number;
  /** Rectangles that fall outside the size announced before them. */
  outside: Rect[];
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
    outside: [],
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
          seen.connected = {
            render: message.render,
            passthrough: message.passthrough,
          };
        }
      });
      return;
    }
    if (url.pathname !== "/ws/display") {
      return;
    }
    // A display socket is an attachment of its own, whose batches and
    // acknowledgments are numbered from one, and whose picture starts with it.
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
        if (message.type === "adaptiveStart") {
          started = true;
        }
        if (message.type === "resize") {
          seen.size = { w: message.w, h: message.h };
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
        seen.unannounced += batch.rects.length;
      }
      const size = seen.size;
      seen.outside.push(
        ...batch.rects.filter(
          (r) => !size || r.x + r.w > size.w || r.y + r.h > size.h,
        ),
      );
      seen.batches.push(batch);
    });
  });
  return seen;
}

const rects = (seen: Session): Rect[] => seen.batches.flatMap((b) => b.rects);

test.describe("a Standard Mac whose Adaptive picture is passed", () => {
  test.skip(
    !ADAPTIVE_TARGET,
    "set REMOTEX_PLAYWRIGHT_ADAPTIVE_TARGET=<target> against a gateway with a live Mac",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("is sent the Mac's rectangles, and decodes every batch of them", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, ADAPTIVE_TARGET ?? "", "", {
      passthrough: "apple-adaptive",
    });

    await expect
      .poll(() => rects(seen).filter((r) => r.kind === "full").length, {
        timeout: 20_000,
      })
      .toBeGreaterThan(0);
    // Decoded, and not merely received: the acknowledgment is sent when the paint
    // worker has finished a batch.
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    expect(seen.connected).toEqual({
      render: "the Mac's Adaptive picture, passed through",
      passthrough: "apple-adaptive",
    });
    expect(seen.badKinds, "binary frames that were not batches").toEqual([]);
    expect(
      seen.unannounced,
      "rectangles that arrived before their socket's adaptiveStart",
    ).toBe(0);
    expect(
      seen.controlTypes,
      "a passed Adaptive picture is no video stream",
    ).not.toContain("videoFormat");
    expect(
      seen.controlTypes.indexOf("resize"),
      "the framebuffer's size is announced ahead of the picture drawn on it",
    ).toBeLessThan(seen.controlTypes.indexOf("adaptiveStart"));

    // The Mac's tables come before its first picture, and are exactly a table each.
    const first = rects(seen)[0];
    expect(first?.kind).toBe("tables");
    expect(first?.length).toBe(TABLES_LEN);
    for (const batch of seen.batches) {
      expect(batch.flags, "reserved frame flags must be zero").toBe(0);
      expect(batch.badOp, "every record must be an ADAPTIVE record").toBe(
        undefined,
      );
      expect(batch.exact, "records must exactly fill the frame").toBe(true);
      expect(
        batch.rects.length,
        "the header's record count must match the records present",
      ).toBe(batch.count);
    }
    expect(
      rects(seen).filter((r) => r.kind === "unknown"),
      "every body is a full update, a partial one or the tables",
    ).toEqual([]);
    expect(seen.outside, "rectangles off the framebuffer").toEqual([]);
    // Acknowledged in the order they were sent, none twice and none invented.
    const sent = seen.batches.map((batch) => batch.sequence);
    expect(sent).toEqual(sent.map((_, index) => index + 1));
    expect(seen.acknowledged).toEqual(
      seen.acknowledged.map((_, index) => index + 1),
    );
    expect(Math.max(...seen.acknowledged)).toBeLessThanOrEqual(
      Math.max(...sent),
    );

    // A rectangle that did not decode says so where the desktop is.
    await expect(page.getByRole("alert")).toHaveCount(0);

    // And the session card says what the picture is.
    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Info", exact: true }).click();
    const card = page.getByRole("dialog", { name: "Info" });
    await expect(card).toContainText("decoded by this page");
    await expect(card).toContainText(
      "the Mac's Adaptive picture, passed through",
    );
    await page.keyboard.press("Escape");
    await expect(card).toHaveCount(0);
  });

  test("starts the picture over for a page that comes back", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, ADAPTIVE_TARGET ?? "", "", {
      passthrough: "apple-adaptive",
    });
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    // The page that comes back holds nothing the Mac draws on, so what it is given
    // is a session from its first rectangle: connected, a picture that starts,
    // and the Mac's tables before anything else.
    const before = seen.controlTypes.length;
    await page.reload();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    const after = seen.controlTypes.slice(before);
    expect(after).toContain("connected");
    expect(after).toContain("adaptiveStart");
    expect(seen.unannounced).toBe(0);
    expect(seen.batches[0]?.sequence).toBe(1);
    expect(rects(seen)[0]?.kind).toBe("tables");
    await expect(page.getByRole("alert")).toHaveCount(0);

    await returnToPicker(page);
  });
});
