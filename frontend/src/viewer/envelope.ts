// The downstream stream's framing.
//
// A chunked HTTP body arrives in whatever pieces the socket produced, which
// have nothing to do with message boundaries: one read can hold three messages
// and half of a fourth, and a 256 KB tile batch arrives across dozens. So every
// message carries its own length.
//
//   [u32 be length][u8 kind][payload...]     length counts the kind byte too
//
// Big-endian because this framing is ours rather than the gateway's, and every
// other length on a wire is; the gateway's own frames (little-endian, see
// protocol.ts) travel through here as opaque payload.

export interface Envelope {
  kind: number;
  /** A copy, not a view: the reader's buffer is reused. */
  payload: Uint8Array;
}

const LENGTH_LEN = 4;

/**
 * The largest message this will assemble.
 *
 * A batch is bounded at 256 KB by `MAX_BATCH_BYTES` in src/wire.rs, but a single
 * tile larger than that is emitted in a frame of its own, so the ceiling here is
 * the socket's rather than the batcher's — matching
 * `URLSessionWebSocketTransport.maximumMessageSize` in the viewer. Past it the
 * length is not a length, and continuing would mean allocating on a number that
 * came out of a corrupt stream.
 */
export const MAX_ENVELOPE_BYTES = 16 << 20;

export class EnvelopeError extends Error {}

export interface EnvelopeReader {
  /** Complete messages this chunk finished, in order. */
  push(chunk: Uint8Array): Envelope[];
}

export function createEnvelopeReader(): EnvelopeReader {
  // Held whole rather than as a chunk list: messages are small enough that the
  // copy is cheaper than the bookkeeping, and every payload has to be
  // contiguous to be parsed anyway.
  let buffer = new Uint8Array(0);

  return {
    push(chunk) {
      if (chunk.byteLength > 0) {
        const grown = new Uint8Array(buffer.byteLength + chunk.byteLength);
        grown.set(buffer, 0);
        grown.set(chunk, buffer.byteLength);
        buffer = grown;
      }
      const out: Envelope[] = [];
      let at = 0;
      while (buffer.byteLength - at >= LENGTH_LEN) {
        const view = new DataView(
          buffer.buffer,
          buffer.byteOffset + at,
          LENGTH_LEN,
        );
        const length = view.getUint32(0, false);
        if (length < 1 || length > MAX_ENVELOPE_BYTES) {
          throw new EnvelopeError(`envelope length ${length}`);
        }
        if (buffer.byteLength - at - LENGTH_LEN < length) {
          break;
        }
        const start = at + LENGTH_LEN;
        out.push({
          kind: buffer[start],
          payload: buffer.slice(start + 1, start + length),
        });
        at = start + length;
      }
      // Only the remainder is carried forward. `subarray` would keep the whole
      // consumed buffer alive behind it, which after one full repaint is
      // megabytes held for a few trailing bytes.
      buffer = at === 0 ? buffer : buffer.slice(at);
      return out;
    },
  };
}
