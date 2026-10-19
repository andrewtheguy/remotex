// RDP 8.0 bulk compression, unwrapped for a spec that reads a passed graphics
// pipeline off the socket: a GRAPHICS record is a message as the host sent it, and
// the commands are inside.
//
// Written from [MS-RDPEGFX] 2.2.5 and 3.1.9.1 and not from the gateway's decoder
// (crates/remotex-rdp-graphics/src/proto/zgfx.rs), so a spec that reads the
// commands with it checks the wire against the specification and not the gateway
// against itself.

/// `RDP_SEGMENTED_DATA`'s descriptor, 2.2.5.1.
const SINGLE = 0xe0;
const MULTIPART = 0xe1;
/// `RDP8_BULK_ENCODED_DATA`'s header, 2.2.5.3.
const PACKET_COMPRESSED = 0x20;
/// 3.1.9.1.2: the longest match distance, which is the history a decoder keeps.
const HISTORY = 2_500_000;

/// The literals with a short code of their own, 3.1.9.1.2.4: prefix, its length
/// in bits, and the byte.
const SHORT_LITERALS: [number, number, number][] = [
  [0b11000, 5, 0x00],
  [0b11001, 5, 0x01],
  [0b110100, 6, 0x02],
  [0b110101, 6, 0x03],
  [0b110110, 6, 0xff],
  [0b1101110, 7, 0x04],
  [0b1101111, 7, 0x05],
  [0b1110000, 7, 0x06],
  [0b1110001, 7, 0x07],
  [0b1110010, 7, 0x08],
  [0b1110011, 7, 0x09],
  [0b1110100, 7, 0x0a],
  [0b1110101, 7, 0x0b],
  [0b1110110, 7, 0x3a],
  [0b1110111, 7, 0x3b],
  [0b1111000, 7, 0x3c],
  [0b1111001, 7, 0x3d],
  [0b1111010, 7, 0x3e],
  [0b1111011, 7, 0x3f],
  [0b1111100, 7, 0x40],
  [0b1111101, 7, 0x80],
  [0b11111100, 8, 0x0c],
  [0b11111101, 8, 0x38],
  [0b11111110, 8, 0x39],
  [0b11111111, 8, 0x66],
];

/// The match distances, 3.1.9.1.2.4: prefix, its length in bits, how many value
/// bits follow, and the distance they are added to. Distance zero is no match but
/// a run of unencoded bytes.
const DISTANCES: [number, number, number, number][] = [
  [0b10001, 5, 5, 0],
  [0b10010, 5, 7, 32],
  [0b10011, 5, 9, 160],
  [0b10100, 5, 10, 672],
  [0b10101, 5, 12, 1696],
  [0b101100, 6, 14, 5792],
  [0b101101, 6, 15, 22176],
  [0b1011100, 7, 18, 54944],
  [0b1011101, 7, 20, 317088],
  [0b10111100, 8, 20, 1365664],
  [0b10111101, 8, 21, 2414240],
];

/// The bits of one compressed segment, most significant first, up to the count
/// its last byte leaves (3.1.9.1.2.4).
class Bits {
  private at = 0;
  private readonly end: number;

  constructor(private readonly data: Uint8Array) {
    if (data.length === 0) {
      throw new Error("a compressed segment with no trailer byte");
    }
    // The five high bits of the trailer are reserved.
    this.end = (data.length - 1) * 8 - (data[data.length - 1] & 0x07);
  }

  get left(): number {
    return this.end - this.at;
  }

  /// The next `count` bits as a number, without taking them; zeros past the end.
  peek(count: number): number {
    let value = 0;
    for (let i = 0; i < count; i += 1) {
      const bit = this.at + i;
      const set =
        bit < this.end && (this.data[bit >> 3] >> (7 - (bit & 7))) & 1;
      value = value * 2 + (set ? 1 : 0);
    }
    return value;
  }

  take(count: number): number {
    if (count > this.left) {
      throw new Error("a compressed segment that ends inside a token");
    }
    const value = this.peek(count);
    this.at += count;
    return value;
  }

  /// The rest of the current byte skipped, and `count` whole bytes taken.
  bytes(count: number): Uint8Array {
    this.at = Math.ceil(this.at / 8) * 8;
    if (count * 8 > this.left) {
      throw new Error("an unencoded run longer than its segment");
    }
    const start = this.at >> 3;
    this.at += count * 8;
    return this.data.subarray(start, start + count);
  }
}

/// One channel's decoder: the history is the channel's, so one is made where a
/// pipeline starts and every message after it goes through it, in order.
export class Zgfx {
  private readonly history = new Uint8Array(HISTORY);
  private cursor = 0;
  private written = 0;

  /// One `RDP_SEGMENTED_DATA`, unwrapped. Throws for one that is not well formed.
  unwrap(message: Uint8Array): Uint8Array {
    if (message.length === 0) {
      throw new Error("an empty message");
    }
    const out: number[] = [];
    const view = new DataView(
      message.buffer,
      message.byteOffset,
      message.byteLength,
    );
    if (message[0] === SINGLE) {
      this.segment(message.subarray(1), out);
    } else if (message[0] === MULTIPART) {
      const count = view.getUint16(1, true);
      const total = view.getUint32(3, true);
      let at = 7;
      for (let i = 0; i < count; i += 1) {
        const size = view.getUint32(at, true);
        this.segment(message.subarray(at + 4, at + 4 + size), out);
        at += 4 + size;
      }
      if (at !== message.length || out.length !== total) {
        throw new Error(
          `a multipart message of ${out.length} bytes that says ${total}`,
        );
      }
    } else {
      throw new Error(`a descriptor of ${message[0]}`);
    }
    return Uint8Array.from(out);
  }

  private segment(segment: Uint8Array, out: number[]): void {
    if (segment.length === 0) {
      throw new Error("a segment with no header");
    }
    const data = segment.subarray(1);
    if ((segment[0] & PACKET_COMPRESSED) === 0) {
      for (const byte of data) {
        this.emit(byte, out);
      }
      return;
    }
    const bits = new Bits(data);
    while (bits.left > 0) {
      this.token(bits, out);
    }
  }

  /// One token of a compressed segment: a literal, a match, or a run of bytes
  /// sent as they are.
  private token(bits: Bits, out: number[]): void {
    if (bits.peek(1) === 0) {
      bits.take(1);
      this.emit(bits.take(8), out);
      return;
    }
    const literal = SHORT_LITERALS.find(
      ([code, length]) => bits.peek(length) === code && length <= bits.left,
    );
    if (literal) {
      bits.take(literal[1]);
      this.emit(literal[2], out);
      return;
    }
    const token = DISTANCES.find(
      ([code, length]) => bits.peek(length) === code && length <= bits.left,
    );
    if (!token) {
      throw new Error("a reserved token");
    }
    const [, length, valueBits, base] = token;
    bits.take(length);
    const distance = base + bits.take(valueBits);
    if (distance === 0) {
      for (const byte of bits.bytes(bits.take(15))) {
        this.emit(byte, out);
      }
      return;
    }
    this.copy(distance, matchLength(bits), out);
  }

  private emit(byte: number, out: number[]): void {
    out.push(byte);
    this.history[this.cursor] = byte;
    this.cursor = (this.cursor + 1) % HISTORY;
    this.written += 1;
  }

  private copy(distance: number, count: number, out: number[]): void {
    if (distance > this.written) {
      throw new Error(`a match ${distance} back into ${this.written} bytes`);
    }
    // Byte by byte, so a match longer than its distance repeats what it copies.
    for (let i = 0; i < count; i += 1) {
      this.emit(
        this.history[(this.cursor - distance + HISTORY) % HISTORY],
        out,
      );
    }
  }
}

/// The length after a match's distance, 3.1.9.1.2.4: `k` ones and a zero, then
/// `k + 1` value bits over a base of `2 ** (k + 1)`; a lone zero is three.
function matchLength(bits: Bits): number {
  let ones = 0;
  while (bits.take(1) === 1) {
    ones += 1;
    if (ones > 14) {
      throw new Error("a match length prefix past the table");
    }
  }
  if (ones === 0) {
    return 3;
  }
  return 2 ** (ones + 1) + bits.take(ones + 1);
}
