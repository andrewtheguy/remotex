// What a target puts on the session socket: the whole desktop as one VP9 stream, so
// everything here is decidable without asking the browser anything: the gateway
// announces `videoFormat` before the stream's first access unit. Nothing here
// looks at a pixel; a VIDEO record is a header this file parses for itself.
//
// It needs a gateway whose local config hard-codes the targets. Keep that
// gitignored file under `tmp/`, for example `tmp/qa_video.toml`:
//
//     cargo run -- serve --config tmp/qa_video.toml
//
//     REMOTEX_PLAYWRIGHT_BASE_URL=http://127.0.0.1:52889/ \
//     REMOTEX_PLAYWRIGHT_USERNAME=admin \
//     REMOTEX_PLAYWRIGHT_PASSWORD=… \
//     REMOTEX_PLAYWRIGHT_VIDEO_TARGET=video \
//     npx playwright test video-stream
import { expect, type Page, test } from "@playwright/test";

import { leaveSession, logInAndConnectTo } from "./support";

/// The opt-in, and the target name in one — the same bargain the audio spec makes.
/// Its presence is the claim that this gateway has a live target to stream; without
/// one the spec would be asserting against a picture that never arrives.
const VIDEO_TARGET = process.env.REMOTEX_PLAYWRIGHT_VIDEO_TARGET;

/// The wire, copied from src/protocol.rs rather than imported from the SPA: this spec
/// is the independent check that the gateway and the client agree, and reading the
/// client's own parser to decide that would be asking the accused.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_VIDEO = 0x03;
const VIDEO_HEADER_LEN = 10;
const VIDEO_KEYFRAME = 0x01;

interface VideoRecord {
  flags: number;
  keyframe: boolean;
  w: number;
  h: number;
  payloadLen: number;
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  video: VideoRecord[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
}

function parseBatch(payload: Buffer): Batch {
  const count = payload.readUInt16LE(2);
  const video: VideoRecord[] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let badOp: number | undefined;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    if (op !== OP_VIDEO) {
      // An op this parser does not know stops it here, which is where the bad byte
      // is. Reading on would take a length out of somebody else's bytes and fail
      // further along, looking like truncation instead.
      badOp = op;
      exact = false;
      break;
    }
    if (at + VIDEO_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const flags = payload.readUInt8(at + 1);
    const payloadLen = payload.readUInt32LE(at + 6);
    video.push({
      flags,
      keyframe: (flags & VIDEO_KEYFRAME) !== 0,
      w: payload.readUInt16LE(at + 2),
      h: payload.readUInt16LE(at + 4),
      payloadLen,
    });
    at += VIDEO_HEADER_LEN + payloadLen;
  }
  return {
    flags: payload.readUInt8(1),
    count,
    sequence: payload.readUInt32LE(4),
    video,
    exact: exact && at === payload.length,
    badOp,
  };
}

interface Session {
  /** Every control message's `type`, in arrival order. */
  controlTypes: string[];
  connected?: { render: string };
  formats: string[];
  /** The last `resize`, which every unit after it must match. */
  resize?: { w: number; h: number };
  batches: Batch[];
  /** Binary frames that were not batches — audio has a socket of its own. */
  badKinds: number[];
  /**
   * Units that arrived before any format said how to decode them. A decoder
   * configured afterwards has already thrown the frame away, so this must stay 0.
   */
  unannounced: number;
  /** Units whose size was not the desktop the last `resize` announced. */
  missized: number;
}

/// Watch the session socket. Registered before navigation, so nothing is missed.
function watchSession(page: Page): Session {
  const seen: Session = {
    controlTypes: [],
    formats: [],
    batches: [],
    badKinds: [],
    unannounced: 0,
    missized: 0,
  };
  page.on("websocket", (ws) => {
    if (new URL(ws.url()).pathname !== "/ws") {
      return;
    }
    ws.on("framereceived", ({ payload }) => {
      if (typeof payload === "string") {
        const message = JSON.parse(payload);
        if (typeof message.type !== "string") {
          return;
        }
        seen.controlTypes.push(message.type);
        if (message.type === "connected") {
          seen.connected = { render: message.render };
        }
        if (message.type === "videoFormat") {
          seen.formats.push(message.decode);
        }
        if (message.type === "resize") {
          seen.resize = { w: message.w, h: message.h };
        }
        return;
      }
      const kind = payload.readUInt8(0);
      if (kind !== BATCH_FRAME_KIND) {
        seen.badKinds.push(kind);
        return;
      }
      const batch = parseBatch(payload);
      for (const unit of batch.video) {
        if (seen.formats.length === 0) {
          seen.unannounced += 1;
        }
        if (
          !seen.resize ||
          unit.w !== seen.resize.w ||
          unit.h !== seen.resize.h
        ) {
          seen.missized += 1;
        }
      }
      seen.batches.push(batch);
    });
  });
  return seen;
}

const units = (seen: Session): VideoRecord[] =>
  seen.batches.flatMap((b) => b.video);

/// Every assertion that holds for the stream.
function assertTheEnvelopeHolds(seen: Session): void {
  expect(seen.badKinds, "binary frames that were not batches").toEqual([]);
  expect(
    seen.unannounced,
    "units that arrived before any videoFormat",
  ).toBe(0);
  expect(
    seen.missized,
    "units that were not the desktop the last resize announced",
  ).toBe(0);

  for (const batch of seen.batches) {
    expect(batch.flags, "reserved frame flags must be zero").toBe(0);
    expect(batch.badOp, "every record op must be one this build knows").toBe(
      undefined,
    );
    expect(batch.exact, "records must exactly fill the frame").toBe(true);
    expect(
      batch.video.length,
      "the header's record count must match the records present",
    ).toBe(batch.count);
  }
  expect(
    seen.batches.map((batch) => batch.sequence),
    "screen batch sequences must increase in socket order",
  ).toEqual(seen.batches.map((_, index) => index + 1));

  for (const unit of units(seen)) {
    expect(unit.flags & ~VIDEO_KEYFRAME, "undefined record flag bits").toBe(0);
    expect(unit.payloadLen).toBeGreaterThan(0);
  }
  // Nothing before it to decode from: a stream that opened on a delta frame is a
  // desktop that never paints, whatever the decoder does.
  expect(units(seen)[0]?.keyframe, "the stream opened without a keyframe").toBe(
    true,
  );
}

test.describe("a video target", () => {
  test.skip(
    !VIDEO_TARGET,
    "set REMOTEX_PLAYWRIGHT_VIDEO_TARGET=<target> against a gateway with a live target",
  );

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("streams announced VP9 access units the client can parse", async ({
    page,
  }) => {
    const seen = watchSession(page);
    await logInAndConnectTo(page, VIDEO_TARGET ?? "");

    await expect
      .poll(() => units(seen).length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    expect(seen.connected?.render).toMatch(/^video q/);
    expect(seen.formats.length).toBeGreaterThan(0);
    for (const format of seen.formats) {
      // The exact WebCodecs string, whose level comes from the picture size — which
      // is why the gateway sends it and the client does not derive it.
      expect(format).toMatch(/^vp09\.\d{2}\.\d{2}\.\d{2}(\.\d{2}){5}$/);
    }
    assertTheEnvelopeHolds(seen);
  });
});
