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
const VIDEO_HEADER_LEN = 10;
const VIDEO_KEYFRAME = 0x01;

interface Unit {
  flags: number;
  w: number;
  h: number;
  payloadLen: number;
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  units: Unit[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
}

// Parse a batch frame the same way `decodeBatchFrame` does, but independently:
// re-using the SPA's parser here would let a wrong parser agree with itself.
function parseBatch(payload: Buffer): Batch {
  const count = payload.readUInt16LE(2);
  const units: Unit[] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  let badOp: number | undefined;
  while (at < payload.length) {
    const op = payload.readUInt8(at);
    // An op this parser does not know stops it here, which is where the bad byte
    // is. Both real parsers reject rather than guess: `decodeBatchFrame` drops the
    // whole frame, and `batch_units` in tests/common/mod.rs refuses the op.
    if (op !== OP_VIDEO) {
      badOp = op;
      exact = false;
      break;
    }
    if (at + VIDEO_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const payloadLen = payload.readUInt32LE(at + 6);
    units.push({
      flags: payload.readUInt8(at + 1),
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
    units,
    exact: exact && at === payload.length,
    badOp,
  };
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
      // The page opens two sockets, and only one of them carries pixels: sound has
      // `/ws/audio` to itself. Watching both would fail the assertion below on an
      // audio frame's 0x03, which would be the wrong reading entirely — audio never
      // appearing here is precisely what this change bought.
      if (new URL(ws.url()).pathname !== "/ws") {
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
      expect(batch.flags, "reserved flags must be zero").toBe(0);
      expect(
        batch.units.length,
        "the header's record count must match the records present",
      ).toBe(batch.count);
      expect(
        batch.badOp,
        "every record op must be one this build knows",
      ).toBeUndefined();
      expect(batch.exact, "records must exactly fill the frame").toBe(true);
      for (const unit of batch.units) {
        expect(
          unit.flags & ~VIDEO_KEYFRAME,
          "the keyframe bit is the only flag",
        ).toBe(0);
        expect(unit.w).toBeGreaterThan(0);
        expect(unit.h).toBeGreaterThan(0);
        expect(unit.payloadLen).toBeGreaterThan(0);
      }
    }

    // The stream starts where a decoder that has seen nothing can start.
    expect(
      batches[0].units[0].flags & VIDEO_KEYFRAME,
      "the first unit of the session is a keyframe",
    ).toBe(VIDEO_KEYFRAME);

    expect(
      batches.map((batch) => batch.sequence),
      "screen batch sequences must increase in socket order",
    ).toEqual(batches.map((_, index) => index + 1));
  });
});
