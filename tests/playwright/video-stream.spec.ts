// What a `render_type = "video"` target puts on the session socket. Video is VP9
// only, so everything here is decidable without asking the browser anything: the
// gateway announces `videoFormat` before a stream's first access unit. Nothing here
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

import { logInAndConnectTo, returnToPicker } from "./support";

/// The opt-in, and the target name in one — the same bargain the audio spec makes.
/// Its presence is the claim that this gateway has a target which streams video;
/// without one the spec would be asserting against a picture that never
/// arrives, and would pass for the wrong reason.
const VIDEO_TARGET = process.env.REMOTEX_PLAYWRIGHT_VIDEO_TARGET;

/// The wire, copied from src/protocol.rs rather than imported from the SPA: this spec
/// is the independent check that the gateway and the client agree, and reading the
/// client's own parser to decide that would be asking the accused.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_TILE = 0x01;
const OP_TILE_REF = 0x02;
const OP_VIDEO = 0x03;
const TILE_HEADER_LEN = 16;
const TILE_REF_LEN = 7;
const VIDEO_HEADER_LEN = 15;
const VIDEO_KEYFRAME = 0x01;
/// `batch::MAX_STREAMS` — the range the wire's `stream` byte may name. Deliberately
/// not `regions::MAX_STREAMS` (4), which bounds how many streams are *live* at once
/// and not which ids they get: a retune holds the outgoing and incoming sets in hand
/// together, so a stream built during one can be handed an id above the live cap and
/// keep it for its whole life.
const WIRE_STREAMS = 16;

interface VideoRecord {
  stream: number;
  flags: number;
  keyframe: boolean;
  x: number;
  y: number;
  w: number;
  h: number;
  payloadLen: number;
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  /** Every record, video or not — the count is the header's claim to check. */
  records: number;
  video: VideoRecord[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
}

/// Parse a batch frame, knowing all three ops. A `video` target sends VIDEO records
/// and, when a stream's rectangle shrinks, TILE records cleaning up what it no longer
/// covers — so a parser that knew only one of them would stop at the other and report
/// it as a truncated frame.
function parseBatch(payload: Buffer): Batch {
  const count = payload.readUInt16LE(2);
  const video: VideoRecord[] = [];
  let records = 0;
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let badOp: number | undefined;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    if (op === OP_TILE_REF) {
      if (at + TILE_REF_LEN > payload.length) {
        exact = false;
        break;
      }
      records += 1;
      at += TILE_REF_LEN;
      continue;
    }
    if (op === OP_TILE) {
      if (at + TILE_HEADER_LEN > payload.length) {
        exact = false;
        break;
      }
      records += 1;
      at += TILE_HEADER_LEN + payload.readUInt32LE(at + 12);
      continue;
    }
    if (op === OP_VIDEO) {
      if (at + VIDEO_HEADER_LEN > payload.length) {
        exact = false;
        break;
      }
      const flags = payload.readUInt8(at + 2);
      const payloadLen = payload.readUInt32LE(at + 11);
      video.push({
        stream: payload.readUInt8(at + 1),
        flags,
        keyframe: (flags & VIDEO_KEYFRAME) !== 0,
        x: payload.readUInt16LE(at + 3),
        y: payload.readUInt16LE(at + 5),
        w: payload.readUInt16LE(at + 7),
        h: payload.readUInt16LE(at + 9),
        payloadLen,
      });
      records += 1;
      at += VIDEO_HEADER_LEN + payloadLen;
      continue;
    }
    // An op this parser does not know stops it here, which is where the bad byte is.
    // Reading on as if it were something else would take a length out of somebody
    // else's bytes and fail further along, looking like truncation instead.
    badOp = op;
    exact = false;
    break;
  }
  return {
    flags: payload.readUInt8(1),
    count,
    sequence: payload.readUInt32LE(4),
    records,
    video,
    exact: exact && at === payload.length,
    badOp,
  };
}

interface VideoFormat {
  stream: number;
  decode: string;
}

interface Session {
  /** Every control message's `type`, in arrival order. */
  controlTypes: string[];
  connected?: { render: string };
  formats: VideoFormat[];
  /**
   * Stream ids the gateway has said are over, in arrival order. A `render_type =
   * "video"` target never ends its one stream, so this is empty there; under
   * `render_motion_subtype = "stream"` it is how a client learns it may let a decoder
   * — and the platform decode session behind it — go.
   */
  ends: number[];
  batches: Batch[];
  /** Binary frames that were not batches — audio has a socket of its own. */
  badKinds: number[];
  /**
   * Streams whose first access unit arrived before the format that says how to decode
   * it. A decoder configured afterwards has already thrown the frame away, so this
   * must stay empty.
   */
  unannounced: number[];
}

/// Watch the session socket. Registered before navigation, so nothing is missed.
function watchSession(page: Page): Session {
  const seen: Session = {
    controlTypes: [],
    formats: [],
    ends: [],
    batches: [],
    badKinds: [],
    unannounced: [],
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
          seen.formats.push({
            stream: message.stream,
            decode: message.decode,
          });
        }
        if (message.type === "videoEnd") {
          // Forgotten, which is what makes `unannounced` below an assertion about
          // `videoEnd` too: the id is free from here, and the next region to be given
          // it must announce a format of its own before its first unit — a client
          // that took this message at its word has no decoder left on it.
          seen.formats = seen.formats.filter(
            (format) => format.stream !== message.stream,
          );
          seen.ends.push(message.stream);
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
        if (!seen.formats.some((f) => f.stream === unit.stream)) {
          seen.unannounced.push(unit.stream);
        }
      }
      seen.batches.push(batch);
    });
  });
  return seen;
}

const units = (seen: Session): VideoRecord[] =>
  seen.batches.flatMap((b) => b.video);

/// Every assertion that holds for any video stream.
function assertTheEnvelopeHolds(seen: Session): void {
  expect(seen.badKinds, "binary frames that were not batches").toEqual([]);
  expect(
    seen.unannounced,
    "streams whose first access unit arrived before its videoFormat",
  ).toEqual([]);

  for (const batch of seen.batches) {
    expect(batch.flags, "reserved frame flags must be zero").toBe(0);
    expect(batch.badOp, "every record op must be one this build knows").toBe(
      undefined,
    );
    expect(batch.exact, "records must exactly fill the frame").toBe(true);
    expect(
      batch.records,
      "the header's record count must match the records present",
    ).toBe(batch.count);
  }
  expect(
    seen.batches.map((batch) => batch.sequence),
    "screen batch sequences must increase in socket order",
  ).toEqual(seen.batches.map((_, index) => index + 1));

  const first = new Map<number, VideoRecord>();
  for (const unit of units(seen)) {
    expect(unit.flags & ~VIDEO_KEYFRAME, "undefined record flag bits").toBe(0);
    expect(unit.stream).toBeLessThan(WIRE_STREAMS);
    expect(unit.payloadLen).toBeGreaterThan(0);
    // The coded rectangle is grown to even sides before it reaches an encoder, and
    // src/video.rs calls that a theorem rather than a hope. The wire is where it is
    // observable from outside the gateway.
    expect(unit.w % 2, `stream ${unit.stream} width ${unit.w} is odd`).toBe(0);
    expect(unit.h % 2, `stream ${unit.stream} height ${unit.h} is odd`).toBe(0);
    expect(unit.x % 2).toBe(0);
    expect(unit.y % 2).toBe(0);
    if (!first.has(unit.stream)) {
      first.set(unit.stream, unit);
    }
  }
  for (const [stream, unit] of first) {
    // Nothing before it to decode from: a stream that opened on a delta frame is a
    // region that never paints, whatever the decoder does.
    expect(unit.keyframe, `stream ${stream} opened without a keyframe`).toBe(
      true,
    );
  }
}

test.describe("a video target", () => {
  test.skip(
    !VIDEO_TARGET,
    "set REMOTEX_PLAYWRIGHT_VIDEO_TARGET=<target> against a gateway with a video target",
  );

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
      expect(format.decode).toMatch(/^vp09\.\d{2}\.\d{2}\.\d{2}$/);
    }
    assertTheEnvelopeHolds(seen);

    // One stream for the whole desktop is what this dial *is*: `video` sends no
    // per-region streams, so a second id here would mean the motion dial ran.
    expect(new Set(units(seen).map((u) => u.stream))).toEqual(new Set([0]));

    await returnToPicker(page);
  });
});
