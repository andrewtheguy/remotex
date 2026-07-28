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
const TILE_HEADER_LEN = 16;
const NO_SLOT = 0xffff;

interface Batch {
  flags: number;
  count: number;
  records: { op: number; format: number; slot: number; payloadLen: number }[];
  /** Whether the records exactly filled the frame. */
  exact: boolean;
}

// Parse a batch frame the same way `decodeBatchFrame` does, but independently:
// re-using the SPA's parser here would let a wrong parser agree with itself.
function parseBatch(payload: Buffer): Batch {
  const count = payload.readUInt16LE(2);
  const records: Batch["records"] = [];
  let at = BATCH_HEADER_LEN;
  let exact = true;
  while (at < payload.length) {
    if (at + TILE_HEADER_LEN > payload.length) {
      exact = false;
      break;
    }
    const payloadLen = payload.readUInt32LE(at + 12);
    records.push({
      op: payload.readUInt8(at),
      format: payload.readUInt8(at + 1),
      slot: payload.readUInt16LE(at + 2),
      payloadLen,
    });
    at += TILE_HEADER_LEN + payloadLen;
  }
  return { flags: payload.readUInt8(1), count, records, exact: exact && at === payload.length };
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
      expect(batch.exact, "records must exactly fill the frame").toBe(true);
      for (const record of batch.records) {
        expect(record.op).toBe(OP_TILE);
        // 1 = PNG, 2 = JPEG. The Mac agent picks per tile.
        expect([1, 2]).toContain(record.format);
        // Nothing is cached yet, so every tile says so.
        expect(record.slot).toBe(NO_SLOT);
        expect(record.payloadLen).toBeGreaterThan(0);
      }
    }

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
