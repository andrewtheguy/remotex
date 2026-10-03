// What a session started with the graphics pipeline passed puts on the session
// socket, and what the page says it did with it: an RDP host's graphics pipeline,
// passed for the browser to compose, where every other session's picture is a video
// stream.
//
// Everything asserted is decided by the system and not by a machine's timing: the
// render the gateway resolved, the order of `graphicsStart` and the records behind
// it, the records' own framing, the acknowledgment the page sends once its paint
// worker has composed a batch, and the DOM's account of a compositor that refused
// what it was fed and of the canvas the picture is shown on. Nothing here looks at
// a pixel. A GRAPHICS record is a header this
// file parses for itself, and what is inside one is the host's.
//
// It needs a gateway whose local config names a live RDP host, and ticks the
// passthrough under it at the picker. Keep that gitignored file under `tmp/`, for
// example `tmp/qa_egfx.toml`:
//
//     cargo run -- serve --config tmp/qa_egfx.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52890/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_EGFX_TARGET=win \
//     bunx playwright test '/egfx-passthrough\.spec\.ts$'
//
// EXPERIMENTAL: a target with `egfx_h264 = true` is told it may draw with H.264,
// which the page decodes. That is a second opt-in, naming such a target, and it
// needs the host to be playing a video when the spec runs: a host draws H.264
// only for what moves like one.
//
//     REMOTEX_PLAYWRIGHT_EGFX_H264_TARGET=win-h264 \
//     bunx playwright test '/egfx-passthrough\.spec\.ts$'
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo, returnToPicker } from "./support";

/// The opt-in, and the target name in one, as the video spec has it.
const EGFX_TARGET = process.env.REMOTEX_PLAYWRIGHT_EGFX_TARGET;
/// The same for a target whose pipeline may carry H.264.
const EGFX_H264_TARGET = process.env.REMOTEX_PLAYWRIGHT_EGFX_H264_TARGET;

/// What every session here is started with: the pipeline passed, and nothing else.
const PASSED = { passthrough: true };

/// The wire, copied from src/protocol.rs rather than imported from the SPA.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_GRAPHICS = 0x04;
const GRAPHICS_HEADER_LEN = 5;
/// `RDPGFX_HEADER`, [MS-RDPEGFX] 2.2.1.5: the command, its flags, the PDU's length.
const RDPGFX_HEADER_LEN = 8;
/// `RDPGFX_WIRE_TO_SURFACE_PDU_1`, [MS-RDPEGFX] 2.2.2.1, whose body starts with the
/// surface and then the codec, and the three codecs that are H.264.
const CMD_WIRE_TO_SURFACE_1 = 0x0001;
const H264_CODECS = [0x000b, 0x000e, 0x000f];

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  /** Each record's commands' length. */
  runs: number[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
  /** Whether every run was whole commands, by the headers' own lengths. */
  whole: boolean;
  /** How many of its commands draw with H.264. */
  h264: number;
}

/// How many commands of `run`, taken as whole PDUs, draw with H.264.
function h264Commands(run: Buffer): number {
  let found = 0;
  let at = 0;
  while (at + RDPGFX_HEADER_LEN <= run.length) {
    const length = run.readUInt32LE(at + 4);
    if (length < RDPGFX_HEADER_LEN) {
      break;
    }
    if (
      run.readUInt16LE(at) === CMD_WIRE_TO_SURFACE_1 &&
      at + RDPGFX_HEADER_LEN + 4 <= run.length &&
      H264_CODECS.includes(run.readUInt16LE(at + RDPGFX_HEADER_LEN + 2))
    ) {
      found += 1;
    }
    at += length;
  }
  return found;
}

/// Whether `run` is whole PDUs end to end: each header's length taken at its word.
function wholeCommands(run: Buffer): boolean {
  let at = 0;
  while (at < run.length) {
    if (at + RDPGFX_HEADER_LEN > run.length) {
      return false;
    }
    const length = run.readUInt32LE(at + 4);
    if (length < RDPGFX_HEADER_LEN) {
      return false;
    }
    at += length;
  }
  return at === run.length;
}

function parseBatch(payload: Buffer): Batch {
  const count = payload.readUInt16LE(2);
  const runs: number[] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let whole = true;
  let h264 = 0;
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
    whole &&= wholeCommands(payload.subarray(start, start + length));
    h264 += h264Commands(payload.subarray(start, start + length));
    runs.push(length);
    at = start + length;
  }
  return {
    flags: payload.readUInt8(1),
    count,
    sequence: payload.readUInt32LE(4),
    runs,
    exact: exact && at === payload.length,
    badOp,
    whole,
    h264,
  };
}

interface Session {
  /** Every control message's `type`, in arrival order. */
  controlTypes: string[];
  connected?: { render: string };
  batches: Batch[];
  /** Binary frames that were not batches. */
  badKinds: number[];
  /**
   * Runs that arrived on a session before its own `graphicsStart` said a pipeline
   * had begun. Counted over every session watched.
   */
  unannounced: number;
  /** The sequences the page acknowledged, as it sent them. */
  acknowledged: number[];
  /** Whether the page said on its session socket that it decodes H.264. */
  decodesH264?: string | null;
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
      seen.decodesH264 = url.searchParams.get("rdp_h264");
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
    // acknowledgments are numbered from one. Whether its pipeline has been
    // announced is its own too, so the socket before cannot answer for it.
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
    const control = (text: string) => {
      const message = JSON.parse(text);
      if (typeof message.type !== "string") {
        return;
      }
      seen.controlTypes.push(message.type);
      if (message.type === "graphicsStart") {
        started = true;
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
      if (!started) {
        seen.unannounced += batch.runs.length;
      }
      seen.batches.push(batch);
    });
  });
  return seen;
}

const runs = (seen: Session): number[] => seen.batches.flatMap((b) => b.runs);

test.describe("a target that passes its graphics pipeline", () => {
  test.skip(
    !EGFX_TARGET,
    "set REMOTEX_PLAYWRIGHT_EGFX_TARGET=<target> against a gateway with a live RDP host",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("is sent the host's commands, and composes every batch of them", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, EGFX_TARGET ?? "", "", PASSED);

    await expect
      .poll(() => runs(seen).length, { timeout: 20_000 })
      .toBeGreaterThan(0);
    // Composed, and not merely received: the acknowledgment is sent when the paint
    // worker has finished a batch.
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    expect(seen.connected?.render).toBe(
      "the host's graphics pipeline, passed through",
    );
    expect(seen.badKinds, "binary frames that were not batches").toEqual([]);
    expect(
      seen.unannounced,
      "runs that arrived before their session's graphicsStart",
    ).toBe(0);
    expect(
      seen.controlTypes,
      "a passed pipeline is no video stream",
    ).not.toContain("videoFormat");
    expect(
      seen.controlTypes.indexOf("resize"),
      "the desktop's size is announced ahead of the pipeline that draws it",
    ).toBeLessThan(seen.controlTypes.indexOf("graphicsStart"));

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
      expect(batch.whole, "a run is whole commands, never part of one").toBe(
        true,
      );
    }
    for (const length of runs(seen)) {
      expect(length).toBeGreaterThan(0);
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

    // A compositor that refused a command says so where the desktop is.
    await expect(page.getByRole("alert")).toHaveCount(0);

    // The pipeline's picture is drawn on a canvas of its own, which the page
    // shows once the paint worker says a run has been drawn on it. A canvas has
    // no role to be found by: it is the one laid over the desktop's.
    await expect(page.locator("canvas.graphics")).toBeVisible();

    // And the session card says which of the two this browser is doing.
    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Info", exact: true }).click();
    const card = page.getByRole("dialog", { name: "Info" });
    await expect(card).toContainText("composed by this browser");
    await expect(card).toContainText(
      "the host's graphics pipeline, passed through",
    );
    await page.keyboard.press("Escape");
    await expect(card).toHaveCount(0);
  });

  test("starts the pipeline over for a page that comes back", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, EGFX_TARGET ?? "", "", PASSED);
    await expect
      .poll(() => seen.acknowledged.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    // The page that comes back holds nothing the host draws against, so what it is
    // given is a session from its first command: connected, and a pipeline that
    // starts, with nothing resumed in between.
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
    expect(after).toContain("graphicsStart");
    expect(
      seen.unannounced,
      "runs that arrived before their session's graphicsStart",
    ).toBe(0);
    expect(seen.batches[0]?.sequence).toBe(1);
    await expect(page.getByRole("alert")).toHaveCount(0);
    await expect(page.locator("canvas.graphics")).toBeVisible();

    await returnToPicker(page);
  });
});

test.describe("a target whose passed pipeline may carry H.264", () => {
  test.skip(
    !EGFX_H264_TARGET,
    "set REMOTEX_PLAYWRIGHT_EGFX_H264_TARGET=<target> against a gateway with a live RDP host that is playing a video",
  );

  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("is sent the host's H.264, and composes the batches that carry it", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, EGFX_H264_TARGET ?? "", "", PASSED);

    // The host is told it may draw with H.264 only for a page that said it
    // decodes it, and the session says which it is.
    await expect.poll(() => seen.connected?.render, { timeout: 20_000 }).toBe(
      "the host's graphics pipeline with H.264, passed through",
    );
    expect(seen.decodesH264).toBe("true");

    // A batch that carries H.264 is acknowledged only once every access unit in
    // it has been decoded and its run composed: a decoder that gave no picture
    // ends the pipeline instead, and says so where the desktop is.
    const carried = () => seen.batches.find((batch) => batch.h264 > 0);
    await expect
      .poll(() => carried()?.sequence, {
        timeout: 30_000,
        message: "the host drew nothing with H.264: is a video playing on it?",
      })
      .toBeGreaterThan(0);
    const sequence = carried()?.sequence ?? 0;
    await expect
      .poll(() => seen.acknowledged.includes(sequence), { timeout: 20_000 })
      .toBe(true);
    for (const batch of seen.batches) {
      expect(batch.exact, "records must exactly fill the frame").toBe(true);
      expect(batch.whole, "a run is whole commands, never part of one").toBe(
        true,
      );
    }
    await expect(page.getByRole("alert")).toHaveCount(0);
    await expect(page.locator("canvas.graphics")).toBeVisible();

    await returnToPicker(page);
  });
});
