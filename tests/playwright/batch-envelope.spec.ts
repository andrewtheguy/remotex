// The v4 binary envelope, observed from the real SPA's own WebSocket.
//
// This is the only test that watches the browser link as the browser actually
// uses it. The Rust e2e tests drive a raw WebSocket client, and the TypeScript
// unit tests parse frames they built themselves — so a gateway and an
// SPA could agree with their own test fixtures and disagree with each other, and
// nothing would notice until a desktop came up blank.
//
// It is in the headless whitelist because it asserts nothing about paint: no
// canvas pixels, no frame timing, no cursor, no gestures. `framereceived` is a
// deterministic transport event, and every assertion below is either a byte in a
// header or a count of records inside one frame.
import { expect, test } from "@playwright/test";
import { leaveSession, logInAndConnect, skipUnlessLiveMac } from "./support";

// Must match `batch` in src/protocol.rs.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_VIDEO = 0x03;
const OP_STANDARD = 0x05;
const OP_TILE = 0x06;
const VIDEO_HEADER_LEN = 10;
const STANDARD_HEADER_LEN = 13;
const TILE_HEADER_LEN = 14;
const VIDEO_KEYFRAME = 0x01;
const VIDEO_STRIP = 0x02;
const VIDEO_STRIP_INDEX = 0x0c;
const VIDEO_STRIP_ENDS = 0x10;
const TILE_JPEG = 0x01;
const TILE_PNG = 0x02;

/// A record as its header frames it: what the Mac's picture is decides which
/// kind arrives, a passed stream's access units, a Standard Mac's own
/// rectangles or the gateway's tiles.
interface BatchRecordFrame {
  op: number;
  /// A video unit's flags; zero for the rest.
  flags: number;
  /// A tile's image format; zero for the rest.
  format: number;
  w: number;
  h: number;
  payloadLen: number;
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  records: BatchRecordFrame[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
}

/// Where each kind keeps its size and its body's length, and how long its header
/// is.
interface Layout {
  header: number;
  size: number;
  length: number;
}

const LAYOUTS: Record<number, Layout> = {
  [OP_VIDEO]: { header: VIDEO_HEADER_LEN, size: 2, length: 6 },
  [OP_STANDARD]: { header: STANDARD_HEADER_LEN, size: 5, length: 9 },
  [OP_TILE]: { header: TILE_HEADER_LEN, size: 6, length: 10 },
};

/// The record at `at`, framed as `layout` says its kind is.
function readRecord(
  payload: Buffer,
  at: number,
  op: number,
  layout: Layout,
): BatchRecordFrame {
  return {
    op,
    flags: op === OP_VIDEO ? payload.readUInt8(at + 1) : 0,
    format: op === OP_TILE ? payload.readUInt8(at + 1) : 0,
    w: payload.readUInt16LE(at + layout.size),
    h: payload.readUInt16LE(at + layout.size + 2),
    payloadLen: payload.readUInt32LE(at + layout.length),
  };
}

function parseBatch(payload: Buffer): Batch {
  const count = payload.readUInt16LE(2);
  const records: BatchRecordFrame[] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let badOp: number | undefined;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    // An op this parser does not know stops it here, which is where the bad
    // byte is. Both real parsers reject rather than guess: `decodeBatchFrame`
    // drops the whole frame, and `batch_records` in tests/common/mod.rs refuses
    // the op.
    const layout: Layout | undefined = LAYOUTS[op];
    if (!layout) {
      badOp = op;
      exact = false;
      break;
    }
    if (at + layout.header > payload.length) {
      exact = false;
      break;
    }
    const record = readRecord(payload, at, op, layout);
    records.push(record);
    at += layout.header + record.payloadLen;
  }
  return {
    flags: payload.readUInt8(1),
    count,
    sequence: payload.readUInt32LE(4),
    records,
    exact: exact && at === payload.length,
    badOp,
  };
}

/// What holds of every batch: its header, and records that exactly fill it.
function expectFramed(batch: Batch): void {
  expect(batch.flags, "reserved flags must be zero").toBe(0);
  expect(
    batch.records.length,
    "the header's record count must match the records present",
  ).toBe(batch.count);
  expect(
    batch.badOp,
    "every record op must be one this build knows",
  ).toBeUndefined();
  expect(batch.exact, "records must exactly fill the frame").toBe(true);
  for (const record of batch.records) {
    expectRecordFramed(record);
  }
}

/// What holds of every record, whatever its kind.
function expectRecordFramed(record: BatchRecordFrame): void {
  // A unit says whether it is a keyframe and, where the picture comes in
  // strips, which strip it is and whether it ends its frame.
  expect(
    record.flags &
      ~(VIDEO_KEYFRAME | VIDEO_STRIP | VIDEO_STRIP_INDEX | VIDEO_STRIP_ENDS),
    "the keyframe and strip bits are the only flags",
  ).toBe(0);
  if ((record.flags & VIDEO_STRIP) === 0) {
    expect(
      record.flags & (VIDEO_STRIP_INDEX | VIDEO_STRIP_ENDS),
      "a strip's number and end only on a strip",
    ).toBe(0);
  }
  if (record.op === OP_TILE) {
    expect(
      [TILE_JPEG, TILE_PNG],
      "a tile's image is a JPEG or a PNG",
    ).toContain(record.format);
  }
  // A Standard record is the Mac's rectangle as it sent it, which may
  // be empty; a unit and a tile never are. Every record has a body.
  if (record.op !== OP_STANDARD) {
    expect(record.w).toBeGreaterThan(0);
    expect(record.h).toBeGreaterThan(0);
  }
  expect(record.payloadLen).toBeGreaterThan(0);
}

test.describe("v4 batch envelope", () => {
  // Needs the Mac's Screen Sharing service to be up: the frames under test are
  // its screen arriving through the gateway.
  skipUnlessLiveMac();

  // Cleanup, so it runs even when an assertion above threw: see `leaveSession`.
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  test("screen updates arrive as batch frames the SPA can parse", async ({
    page,
  }) => {
    const batches: Batch[] = [];
    const badKinds: number[] = [];

    page.on("websocket", (ws) => {
      // The page opens several sockets, and only the display's carries pixels:
      // sound has `/ws/audio` to itself, and the session socket carries text.
      // Watching the audio one would fail the assertion below on an audio frame's
      // 0x03, which would be the wrong reading entirely — audio never appearing
      // here is precisely what giving it a socket bought.
      if (new URL(ws.url()).pathname !== "/ws/display") {
        return;
      }
      ws.on("framereceived", ({ payload }) => {
        // Text frames are the control channel and stay JSON; only binary frames
        // are the envelope under test.
        if (typeof payload === "string") {
          return;
        }
        const kind = payload.readUInt8(0);
        if (kind !== BATCH_FRAME_KIND) {
          badKinds.push(kind);
          return;
        }
        batches.push(parseBatch(payload));
      });
    });

    await logInAndConnect(page);

    // The gateway repaints on attach, so frames arrive without anything being
    // driven. Poll rather than sleep: web-first, and it stops as soon as enough
    // has arrived to judge.
    await expect
      .poll(() => batches.length, { timeout: 20_000 })
      .toBeGreaterThan(0);

    // Every binary frame is a batch. A v2 frame would lead with 0x01, and the
    // whole point of retiring that kind is that a mismatch is loud.
    expect(badKinds, "binary frames that were not batches").toEqual([]);

    for (const batch of batches) {
      expectFramed(batch);
    }

    // A passed stream starts where a decoder that has seen nothing can start.
    const video = batches
      .flatMap((batch) => batch.records)
      .filter((record) => record.op === OP_VIDEO);
    if (video.length > 0) {
      expect(
        video[0].flags & VIDEO_KEYFRAME,
        "the first unit of the session is a keyframe",
      ).toBe(VIDEO_KEYFRAME);
    }

    expect(
      batches.map((batch) => batch.sequence),
      "screen batch sequences must increase in socket order",
    ).toEqual(batches.map((_, index) => index + 1));
  });
});
