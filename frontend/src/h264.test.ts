// The half of the H.264 path a test can reach. Everything around it is a WebCodecs
// `VideoDecoder`, which does not exist here — and whether a given browser decodes
// what this gateway encodes is not a question a unit test can answer anyway. That is
// browser QA, and the gateway half is pinned by `cargo test --lib h264::`.
//
// What is checked here is the reading: which units are keyframes, and the codec
// string a decoder is configured from. Both go wrong quietly — a delta typed as a
// key produces a decode error, and a wrong codec string produces a decoder that
// configures and then fails — so they are worth pinning a byte at a time.
//
// Run with `bun test src/h264.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { readAccessUnit } from "./h264.ts";

/** A NAL unit of `type` with `body` after its header, behind a start code. */
function nal(type: number, body: number[] = [], long = false): number[] {
  const start = long ? [0, 0, 0, 1] : [0, 0, 1];
  return [...start, type, ...body];
}

function unit(...nals: number[][]): Uint8Array {
  return new Uint8Array(nals.flat());
}

test("a keyframe carries its parameter sets and says a decoder may start", () => {
  // What the gateway actually sends for a keyframe: SPS, PPS, IDR slice.
  const read = readAccessUnit(
    unit(
      nal(7, [0x42, 0xc0, 0x1e], true),
      nal(8, [0xce], true),
      nal(5, [0x88]),
    ),
  );
  assert.deepEqual(read, { key: true, codec: "avc1.42c01e" });
});

test("an ordinary frame is a delta and names no codec", () => {
  assert.deepEqual(readAccessUnit(unit(nal(1, [0x9a]))), {
    key: false,
    codec: null,
  });
});

test("both start code lengths are read, because one stream uses both", () => {
  // openh264 writes four bytes ahead of a parameter set and three elsewhere, so a
  // reader that handled only one length would stop mid-unit.
  const read = readAccessUnit(
    unit(nal(7, [0x64, 0x00, 0x28], true), nal(5, [0x88])),
  );
  assert.deepEqual(read, { key: true, codec: "avc1.640028" });
});

test("the codec string keeps the leading zero of a byte below 16", () => {
  // avc1.4d000a, not avc1.4d0a — a dropped zero is a codec string that names a
  // different profile and level, and `configure` would take it.
  const read = readAccessUnit(
    unit(nal(7, [0x4d, 0x00, 0x0a], true), nal(5, [0x88])),
  );
  assert.equal(read?.codec, "avc1.4d000a");
});

test("payloads that are not an Annex-B stream are refused rather than guessed at", () => {
  // No start code at all — a JPEG, say, or a truncated frame.
  assert.equal(readAccessUnit(new Uint8Array([0xff, 0xd8, 0xff, 0xe0])), null);
  assert.equal(readAccessUnit(new Uint8Array([])), null);
  // A start code and nothing after it.
  assert.equal(readAccessUnit(new Uint8Array([0, 0, 0, 1])), null);
  // The forbidden_zero_bit set: whatever this is, it is not a NAL header.
  assert.equal(readAccessUnit(unit(nal(0x85))), null);
});

test("parameter sets with no slice behind them are not an access unit", () => {
  // Nothing would ever be painted for it, and a pending decode would wait for a
  // frame that is never coming — which stalls the whole message queue.
  assert.equal(
    readAccessUnit(unit(nal(7, [0x42, 0xc0, 0x1e], true), nal(8, [0xce]))),
    null,
  );
});

test("a unit is read to its end rather than stopping at the first slice", () => {
  // The SPS comes first in practice, but the reader must not depend on the order:
  // stopping at the slice would lose a codec string that followed it.
  const read = readAccessUnit(
    unit(nal(5, [0x88]), nal(7, [0x42, 0xc0, 0x1e], true)),
  );
  assert.deepEqual(read, { key: true, codec: "avc1.42c01e" });
});
