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
import { logInAndConnect, returnToPicker, skipUnlessLiveMac } from "./support";

// Must match `batch` in src/protocol.rs.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 8;
const OP_TILE = 0x01;
const OP_TILE_REF = 0x02;
const OP_COPY = 0x04;
const TILE_HEADER_LEN = 16;
const TILE_REF_LEN = 7;
const COPY_LEN = 13;
const NO_SLOT = 0xffff;
const SLOT_COUNT = 256;
const TILE_FORMAT_PNG = 1;
const TILE_FORMAT_JPEG = 2;
const TILE_FORMAT_WEBP = 3;

interface Record {
  op: number;
  slot: number;
  /** Tiles only. */
  format?: number;
  payloadLen?: number;
  /** Copies only: the source and destination rectangles. */
  copy?: { sx: number; sy: number; x: number; y: number; w: number; h: number };
}

interface Batch {
  flags: number;
  count: number;
  sequence: number;
  records: Record[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
  /** The first record op this parser did not recognize, if any. */
  badOp?: number;
}

// Parse a batch frame the same way `decodeBatchFrame` does, but independently:
// re-using the SPA's parser here would let a wrong parser agree with itself.
function parseBatch(payload: Buffer): Batch {
  const count = payload.readUInt16LE(2);
  const records: Record[] = [];
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
      records.push({ op: OP_TILE_REF, slot: payload.readUInt16LE(at + 1) });
      at += TILE_REF_LEN;
      continue;
    }
    // A copy carries no payload and no slot, only the two rectangles. Transcribed
    // even though a Mac never sends one — the encoding lists for both Apple
    // subtypes omit CopyRect, so only generic VNC can produce this — because a
    // parser that is a partial transcription of the wire reports the next record it
    // does not know as a corrupt frame. Both rectangles are kept rather than the
    // fact of the record alone, so the loop below can check a copy the way it
    // checks everything else instead of falling through to the tile rules and
    // failing on a format it does not have.
    if (op === OP_COPY) {
      if (at + COPY_LEN > payload.length) {
        exact = false;
        break;
      }
      records.push({
        op,
        slot: NO_SLOT,
        copy: {
          sx: payload.readUInt16LE(at + 1),
          sy: payload.readUInt16LE(at + 3),
          x: payload.readUInt16LE(at + 5),
          y: payload.readUInt16LE(at + 7),
          w: payload.readUInt16LE(at + 9),
          h: payload.readUInt16LE(at + 11),
        },
      });
      at += COPY_LEN;
      continue;
    }
    // An op this parser does not know stops it here, which is where the bad byte
    // is. Reading on as if it were a TILE would take a payload length out of
    // somebody else's bytes and fail somewhere further along that looks like a
    // truncated frame instead. Both real parsers reject rather than guess:
    // `decodeBatchFrame` drops the whole frame, and `batch_records` in
    // tests/common/mod.rs refuses the op by name.
    if (op !== OP_TILE) {
      badOp = op;
      exact = false;
      break;
    }
    if (at + TILE_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const payloadLen = payload.readUInt32LE(at + 12);
    records.push({
      op,
      format: payload.readUInt8(at + 1),
      slot: payload.readUInt16LE(at + 2),
      payloadLen,
    });
    at += TILE_HEADER_LEN + payloadLen;
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

test.describe("v4 batch envelope", () => {
  // Needs the Mac's Screen Sharing service to be up: the frames under test are
  // its screen arriving through the gateway.
  skipUnlessLiveMac();

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
        batch.records.length,
        "the header's record count must match the records present",
      ).toBe(batch.count);
      expect(
        batch.badOp,
        "every record op must be one this build knows",
      ).toBeUndefined();
      expect(batch.exact, "records must exactly fill the frame").toBe(true);
      for (const record of batch.records) {
        if (record.op === OP_TILE_REF) {
          // A reference names a slot the SPA is keeping. Seven bytes, no payload,
          // and the slot must be inside the fixed cache the wire promises.
          expect(record.slot).toBeLessThan(SLOT_COUNT);
          continue;
        }
        if (record.op === OP_COPY) {
          // Both rectangles are the same size and cover real pixels — a copy of
          // nothing is a record for nothing, and the destination is where the SPA
          // is about to blit.
          const copy = record.copy;
          expect(copy).toBeDefined();
          expect(copy?.w).toBeGreaterThan(0);
          expect(copy?.h).toBeGreaterThan(0);
          continue;
        }
        // The gateway encodes tiles as PNG, JPEG, or WebP according to the target's
        // render dial, so any sample uses one of the three wire formats.
        expect([
          TILE_FORMAT_PNG,
          TILE_FORMAT_JPEG,
          TILE_FORMAT_WEBP,
        ]).toContain(record.format);
        // Either a slot inside the cache, or "do not remember this".
        if (record.slot !== NO_SLOT) {
          expect(record.slot).toBeLessThan(SLOT_COUNT);
        }
        expect(record.payloadLen).toBeGreaterThan(0);
      }
    }

    expect(
      batches.map((batch) => batch.sequence),
      "screen batch sequences must increase in socket order",
    ).toEqual(batches.map((_, index) => index + 1));

    // Whatever the gateway sent, the SPA has to have parsed it: every record it
    // could not read would be a dropped frame, and a dropped frame is a region of
    // stale pixels. The parser above is the check — it would have thrown.
    //
    // The envelope's reason to exist: a repaint of a real desktop puts more than
    // one tile in a frame. Asserted across the run rather than per frame, because
    // a quiet moment legitimately produces a single-record batch.
    const total = batches.reduce((sum, b) => sum + b.records.length, 0);
    expect(
      total,
      `${batches.length} frames carried ${total} records; batching should carry more records than frames`,
    ).toBeGreaterThan(batches.length);

    await returnToPicker(page);
  });
});
