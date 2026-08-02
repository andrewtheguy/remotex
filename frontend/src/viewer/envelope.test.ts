// The downstream framing, over byte splits a socket really produces.
//
// Run with `bun test src/viewer/envelope.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  createEnvelopeReader,
  EnvelopeError,
  MAX_ENVELOPE_BYTES,
} from "./envelope.ts";

function encode(kind: number, payload: number[]): Uint8Array {
  const length = payload.length + 1;
  return new Uint8Array([
    (length >>> 24) & 0xff,
    (length >>> 16) & 0xff,
    (length >>> 8) & 0xff,
    length & 0xff,
    kind,
    ...payload,
  ]);
}

test("a chunk holding whole messages yields them in order", () => {
  const reader = createEnvelopeReader();
  const stream = new Uint8Array([
    ...encode(0x00, [1, 2]),
    ...encode(0x01, [3]),
  ]);
  assert.deepEqual(reader.push(stream), [
    { kind: 0x00, payload: new Uint8Array([1, 2]) },
    { kind: 0x01, payload: new Uint8Array([3]) },
  ]);
});

test("a message split anywhere is assembled, including inside its length", () => {
  // The split that matters most is inside the four length bytes: a reader that
  // peeked before it had all four would read a length from bytes that are not
  // one yet.
  const stream = new Uint8Array([
    ...encode(0x00, [7, 8, 9]),
    ...encode(0x01, [10]),
  ]);
  for (let cut = 1; cut < stream.byteLength; cut += 1) {
    const reader = createEnvelopeReader();
    const first = reader.push(stream.slice(0, cut));
    const second = reader.push(stream.slice(cut));
    assert.deepEqual(
      [...first, ...second],
      [
        { kind: 0x00, payload: new Uint8Array([7, 8, 9]) },
        { kind: 0x01, payload: new Uint8Array([10]) },
      ],
    );
  }
});

test("one byte at a time still assembles", () => {
  const reader = createEnvelopeReader();
  const stream = encode(0x01, [1, 2, 3, 4]);
  const out = [];
  for (const byte of stream) {
    out.push(...reader.push(new Uint8Array([byte])));
  }
  assert.deepEqual(out, [
    { kind: 0x01, payload: new Uint8Array([1, 2, 3, 4]) },
  ]);
});

test("an empty payload is a message, not a gap", () => {
  // `clear` and `audioStop` carry a JSON body, but nothing forbids a zero-length
  // payload arriving, and treating it as "not ready yet" would stall the stream.
  const reader = createEnvelopeReader();
  assert.deepEqual(reader.push(encode(0x00, [])), [
    { kind: 0x00, payload: new Uint8Array([]) },
  ]);
});

test("an empty chunk is not a message", () => {
  const reader = createEnvelopeReader();
  assert.deepEqual(reader.push(new Uint8Array(0)), []);
});

test("a payload is a copy, so the reader's buffer can be reused", () => {
  const reader = createEnvelopeReader();
  const stream = encode(0x01, [1, 2, 3]);
  const [envelope] = reader.push(stream);
  stream.fill(0xff);
  assert.deepEqual(envelope.payload, new Uint8Array([1, 2, 3]));
});

test("a length that cannot be one ends the stream rather than allocating", () => {
  const reader = createEnvelopeReader();
  assert.throws(() => reader.push(new Uint8Array([0, 0, 0, 0])), EnvelopeError);

  const huge = MAX_ENVELOPE_BYTES + 1;
  const overLimit = createEnvelopeReader();
  assert.throws(
    () =>
      overLimit.push(
        new Uint8Array([
          (huge >>> 24) & 0xff,
          (huge >>> 16) & 0xff,
          (huge >>> 8) & 0xff,
          huge & 0xff,
        ]),
      ),
    EnvelopeError,
  );
});
