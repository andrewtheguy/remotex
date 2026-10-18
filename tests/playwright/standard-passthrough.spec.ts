// What a session started with a Standard Mac's Adaptive picture puts on the
// display socket, and what the page says it did with it: the Mac's own 8×8
// tiles, passed for the page to decode. The Mac's Full quality is the gateway's
// tiles, checked in standard-combined.spec.ts.
//
// Everything asserted is decided by the system and not by a machine's timing: the
// render the gateway resolved, the order of `standardStart` and the records
// behind it, the records' own framing, that the first is the Mac's quantization
// tables, the acknowledgment the page sends once its paint worker has decoded a
// batch, and the DOM's account of a rectangle that did not decode. Nothing here
// looks at a pixel. A STANDARD record is a header this file parses for itself
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
//     REMOTEX_PLAYWRIGHT_STANDARD_TARGET=macvmstandard \
//     bunx playwright test '/standard-passthrough\.spec\.ts$'
//
// The resize case needs a target whose desktop follows the window, an `ard`
// target with `virtual_display = true`, named by
// REMOTEX_PLAYWRIGHT_STANDARD_RESIZE_TARGET. The two-screen case needs an `ard`
// target on a Mac with two screens of different densities, a 1x one beside a 2x
// one, named by REMOTEX_PLAYWRIGHT_STANDARD_COMBINED_TARGET.
import { expect, type Page, test, type Worker } from "@playwright/test";

import {
  chooseDisplay,
  faults,
  leaveSession,
  logInAndConnectTo,
  returnToPicker,
  startTarget,
} from "./support";

/// What every session here is started with: the Mac's Adaptive picture, passed.
const ADAPTIVE = { passthrough: "apple-adaptive" } as const;

/// The opt-in, and the target name in one, as the other live specs have it.
const STANDARD_TARGET = process.env.REMOTEX_PLAYWRIGHT_STANDARD_TARGET;
const RESIZE_TARGET = process.env.REMOTEX_PLAYWRIGHT_STANDARD_RESIZE_TARGET;
const COMBINED_TARGET = process.env.REMOTEX_PLAYWRIGHT_STANDARD_COMBINED_TARGET;
/// The id the Mac's combined view is named by on the wire, which a session of
/// one screen at a time never lists.
const COMBINED = 0xffffffff;

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_STANDARD = 0x05;
const STANDARD_HEADER_LEN = 13;
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
  /** The body, a view of the frame it came in, read where it arrives. */
  body?: Buffer;
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
    if (op !== OP_STANDARD) {
      badOp = op;
      exact = false;
      break;
    }
    if (at + STANDARD_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const field = (i: number) => payload.readUInt16LE(at + 1 + 2 * i);
    const length = payload.readUInt32LE(at + 9);
    const start = at + STANDARD_HEADER_LEN;
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
      body: payload.subarray(start, start + length),
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

interface MosaicRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

interface Session {
  /** Every control message's `type`, in arrival order. */
  controlTypes: string[];
  /** The last display list, and the entry being shared. */
  displays?: { active: number; displays: { id: number; label: string }[] };
  /** Every `mosaic`'s regions, in arrival order: empty ends a composition. */
  mosaics: { pixels: MosaicRect; points: MosaicRect }[][];
  /** The last `oversize`'s cause: why the desktop is held, or null. */
  oversize?: string | null;
  connected?: { render: string; passthrough: string | null };
  /** The last size the display socket announced, in pixels. */
  size?: { w: number; h: number };
  batches: Batch[];
  /** Binary frames that were not batches. */
  badKinds: number[];
  /** Rectangles that arrived before their socket's `standardStart`. */
  unannounced: number;
  /** Rectangles that fall outside the size announced before them. */
  outside: Rect[];
  /** The sequences the page acknowledged, as it sent them. */
  acknowledged: number[];
}

/// Keep the display list, the compositions and whether the desktop is held,
/// whichever socket brought them.
function noteDisplays(
  seen: Session,
  message: { type: string; [key: string]: unknown },
): void {
  if (message.type === "displays") {
    seen.displays = message as unknown as NonNullable<Session["displays"]>;
  } else if (message.type === "mosaic") {
    seen.mosaics.push(message.regions as Session["mosaics"][number]);
  } else if (message.type === "oversize") {
    seen.oversize = message.cause as string | null;
  }
}

/// Watch the session socket, and the display socket that carries its picture.
/// Registered before navigation, so nothing is missed.
function watchSession(page: Page): Session {
  const seen: Session = {
    controlTypes: [],
    mosaics: [],
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
        noteDisplays(seen, message);
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
        noteDisplays(seen, message);
        if (message.type === "standardStart") {
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
      for (const rect of batch.rects) {
        // Read where it arrived: not held past it.
        rect.body = undefined;
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
    !STANDARD_TARGET,
    "set REMOTEX_PLAYWRIGHT_STANDARD_TARGET=<target> against a gateway with a live Mac",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("is sent the Mac's rectangles, and decodes every batch of them", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, STANDARD_TARGET ?? "", "", {
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
      "rectangles that arrived before their socket's standardStart",
    ).toBe(0);
    expect(
      seen.controlTypes,
      "a passed Adaptive picture is no video stream",
    ).not.toContain("videoFormat");
    expect(
      seen.controlTypes.indexOf("resize"),
      "the framebuffer's size is announced ahead of the picture drawn on it",
    ).toBeLessThan(seen.controlTypes.indexOf("standardStart"));

    // The Mac's tables come before its first picture, and are exactly a table each.
    const first = rects(seen)[0];
    expect(first?.kind).toBe("tables");
    expect(first?.length).toBe(TABLES_LEN);
    for (const batch of seen.batches) {
      expect(batch.flags, "reserved frame flags must be zero").toBe(0);
      expect(batch.badOp, "every record must be a STANDARD record").toBe(
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
    await expect(faults(page)).toHaveCount(0);

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
    await logInAndConnectTo(page, STANDARD_TARGET ?? "", "", {
      passthrough: "apple-adaptive",
    });
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    // The page that comes back holds nothing the Mac draws on, so what it is given
    // is a session from its first rectangle: connected, a picture that starts,
    // and the Mac's tables before anything else.
    const before = seen.controlTypes.length;
    // The list the first page's display socket filled, which stays `seen`'s
    // until the page that comes back opens a socket of its own.
    const firstAcknowledged = seen.acknowledged;
    await page.reload();
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    await expect
      .poll(
        () =>
          seen.acknowledged !== firstAcknowledged &&
          seen.acknowledged.length > 0,
        { timeout: 20_000 },
      )
      .toBe(true);
    const after = seen.controlTypes.slice(before);
    expect(after).toContain("connected");
    expect(after).toContain("standardStart");
    expect(seen.unannounced).toBe(0);
    expect(seen.batches[0]?.sequence).toBe(1);
    expect(rects(seen)[0]?.kind).toBe("tables");
    await expect(faults(page)).toHaveCount(0);

    await returnToPicker(page);
  });

  test("ends its paint worker with the session, and decodes the next in another", async ({
    page,
  }) => {
    // The decoder is a module of the paint worker's own, and a WebAssembly memory
    // gives nothing back short of its worker ending, so the session's end ends
    // that worker (desktopPainter.ts).
    const painters: Worker[] = [];
    page.on("worker", (worker) => {
      if (/desktopPainter\.worker/.test(worker.url())) {
        painters.push(worker);
      }
    });
    const seen = watchSession(page);
    await logInAndConnectTo(page, STANDARD_TARGET ?? "", "", ADAPTIVE);
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    expect(painters, "one paint worker for the first session").toHaveLength(1);
    const first = painters[0];
    const firstEnded = new Promise<void>((resolve) =>
      first.once("close", () => resolve()),
    );

    await returnToPicker(page);
    await firstEnded;
    // What was acknowledged so far is the first session's. A display socket the
    // next session opens starts a list of its own, and one the picker kept goes
    // on adding to this one.
    const firstAcknowledged = seen.acknowledged;
    const firstAcknowledgedCount = firstAcknowledged.length;
    const firstControl = seen.controlTypes.length;

    await startTarget(page, STANDARD_TARGET ?? "", ADAPTIVE);
    await expect(page.getByRole("button", { name: "Open menu" })).toBeVisible({
      timeout: 20_000,
    });
    // Decoded by the paint worker the second session started, on the canvases
    // mounted for it.
    await expect
      .poll(() => painters.length, {
        message: "a paint worker of the second session's own",
        timeout: 20_000,
      })
      .toBe(2);
    await expect
      .poll(
        () =>
          seen.controlTypes.slice(firstControl).includes("standardStart") &&
          seen.acknowledged.length >
            (seen.acknowledged === firstAcknowledged
              ? firstAcknowledgedCount
              : 0),
        { timeout: 20_000 },
      )
      .toBe(true);
    expect(seen.unannounced).toBe(0);
    await expect(faults(page)).toHaveCount(0);

    await returnToPicker(page);
  });
});

// Ahead of the virtual display's cases: for a while after a session on the
// virtual display, the Mac lists one screen. Combined Display is a session of its
// own (standard-combined.spec.ts), so a session of one screen at a time lists the
// screens alone, starts on the main one and never composes.
test.describe("a Standard Mac of two screens shown one at a time", () => {
  test.skip(
    !COMBINED_TARGET,
    "set REMOTEX_PLAYWRIGHT_STANDARD_COMBINED_TARGET=<target> against a gateway with a live Mac of a 1x and a 2x screen",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("passes its Adaptive picture of the main screen, and of the other once chosen", async ({
    page,
  }) => {
      test.setTimeout(120_000);
      const seen = watchSession(page);
      await logInAndConnectTo(page, COMBINED_TARGET ?? "", "", ADAPTIVE);
      await expect
        .poll(() => seen.displays?.displays.length ?? 0, {
          message: "two screens",
          timeout: 20_000,
        })
        .toBe(2);
      expect(
        seen.displays?.displays.map((display) => display.id),
        "no Combined Display entry",
      ).not.toContain(COMBINED);
      const label = (id: number) =>
        seen.displays?.displays.find((display) => display.id === id)?.label ??
        "";

      // The view the Mac names, then batches painted after it.
      const shows = async (id: number) => {
        await expect
          .poll(() => seen.displays?.active, { timeout: 20_000 })
          .toBe(id);
        const painted = seen.acknowledged.length;
        await expect
          .poll(() => seen.acknowledged.length, {
            message: `a batch of ${label(id)} painted`,
            timeout: 20_000,
          })
          .toBeGreaterThan(painted);
      };
      // The session starts on a screen, never on the combined view.
      await expect
        .poll(() => seen.displays?.active ?? COMBINED, { timeout: 20_000 })
        .not.toBe(COMBINED);
      const first = seen.displays?.active ?? 0;
      await shows(first);
      const other =
        seen.displays?.displays.find((display) => display.id !== first)?.id ??
        0;
      await chooseDisplay(page, label(first), label(other));
      await shows(other);
      await chooseDisplay(page, label(other), label(first));
      await shows(first);

      expect(
        seen.mosaics.every((regions) => regions.length === 0),
        "a screen presented whole",
      ).toBe(true);
      expect(seen.oversize, "a Mac's own picture has no ceiling").toBeUndefined();
      expect(seen.controlTypes).not.toContain("videoFormat");
      expect(seen.outside, "rectangles off the framebuffer").toEqual([]);
      for (const batch of seen.batches) {
        expect(batch.badOp).toBe(undefined);
        expect(batch.exact).toBe(true);
      }

      await returnToPicker(page);
  });
});

test.describe("a Mac's virtual display whose Adaptive picture is passed", () => {
  test.skip(
    !RESIZE_TARGET,
    "set REMOTEX_PLAYWRIGHT_STANDARD_RESIZE_TARGET=<target> against a gateway with a live Mac on a virtual display",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("goes on decoding at the size a resize brings", async ({ page }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, RESIZE_TARGET ?? "", "", {
      passthrough: "apple-adaptive",
      resize: true,
    });
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    const before = seen.size;
    expect(before, "the size announced ahead of the picture").toBeDefined();

    // A window of another size, which a virtual display follows.
    const viewport = page.viewportSize();
    expect(viewport).not.toBeNull();
    await page.setViewportSize({
      width: (viewport?.width ?? 1280) - 160,
      height: (viewport?.height ?? 720) - 120,
    });
    await expect
      .poll(() => seen.size, { timeout: 20_000 })
      .not.toEqual(before);
    const resized = seen.batches.length;
    const acknowledged = seen.acknowledged.length;

    // The Mac draws the new desktop whole, and the page decodes it: its tables
    // and its cache outlive the resize, as the Mac's do, so nothing here asks
    // for them again.
    await expect
      .poll(
        () =>
          seen.batches
            .slice(resized)
            .flatMap((b) => b.rects)
            .filter((r) => r.kind === "full").length,
        { timeout: 20_000 },
      )
      .toBeGreaterThan(0);
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(acknowledged);

    expect(seen.badKinds, "binary frames that were not batches").toEqual([]);
    expect(seen.unannounced).toBe(0);
    for (const batch of seen.batches) {
      expect(batch.badOp).toBe(undefined);
      expect(batch.exact, "records must exactly fill the frame").toBe(true);
    }
    expect(
      rects(seen).filter((r) => r.kind === "unknown"),
      "every body is a full update, a partial one or the tables",
    ).toEqual([]);
    expect(
      seen.outside,
      "rectangles off the framebuffer the last resize announced",
    ).toEqual([]);
    expect(
      seen.controlTypes.filter((type) => type === "standardStart"),
      "a resize goes on with the picture rather than starting it again",
    ).toHaveLength(1);
    expect(seen.acknowledged).toEqual(
      seen.acknowledged.map((_, index) => index + 1),
    );
    await expect(page.getByRole("alert")).toHaveCount(0);

    await returnToPicker(page);
  });
});
