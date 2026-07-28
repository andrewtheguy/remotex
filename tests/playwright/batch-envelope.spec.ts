// The v3 binary envelope, observed from the real SPA's own WebSocket.
//
// This is the only test that watches the browser link as the browser actually
// uses it. The Rust e2e tests drive a raw WebSocket client, and the Swift and
// TypeScript unit tests parse frames they built themselves — so a gateway and an
// SPA could agree with their own test fixtures and disagree with each other, and
// nothing would notice until a desktop came up blank.
//
// It is in the headless whitelist because it asserts nothing about paint: no
// canvas pixels, no frame timing, no cursor, no gestures. `framereceived` is a
// deterministic transport event, and every assertion below is either a byte in a
// header or a count of records inside one frame.
import { expect, test } from "@playwright/test";
import {
  BASE_URL,
  logInAndConnect,
  MISSING_ENV,
  returnToPicker,
} from "./support";

// Must match `batch` in src/protocol.rs.
const BATCH_FRAME_KIND = 0x02;
const BATCH_HEADER_LEN = 4;
const OP_TILE = 0x01;
const OP_TILE_REF = 0x02;
const TILE_HEADER_LEN = 16;
const TILE_REF_LEN = 7;
const NO_SLOT = 0xffff;
const SLOT_COUNT = 256;

interface Record {
  op: number;
  slot: number;
  /** Tiles only. */
  format?: number;
  payloadLen?: number;
}

interface Batch {
  flags: number;
  count: number;
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
    records,
    exact: exact && at === payload.length,
    badOp,
  };
}

test.describe("v3 batch envelope", () => {
  test.skip(
    MISSING_ENV.length > 0,
    `set ${MISSING_ENV.join(", ")} to run the live-Mac specs`,
  );

  test("screen updates arrive as batch frames the SPA can parse", async ({
    page,
  }) => {
    const batches: Batch[] = [];
    const badKinds: number[] = [];

    page.on("websocket", (ws) => {
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
        // 1 = PNG, 2 = JPEG. The Mac agent picks per tile.
        expect([1, 2]).toContain(record.format);
        // Either a slot inside the cache, or "do not remember this".
        if (record.slot !== NO_SLOT) {
          expect(record.slot).toBeLessThan(SLOT_COUNT);
        }
        expect(record.payloadLen).toBeGreaterThan(0);
      }
    }

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

  test("the gateway advertises the protocol version the SPA was built for", async ({
    page,
  }) => {
    await page.goto(BASE_URL);
    const config = await page.evaluate(async () => {
      const response = await fetch("/api/config");
      return (await response.json()) as { protocolVersion: number };
    });
    expect(config.protocolVersion).toBe(3);
  });
});
