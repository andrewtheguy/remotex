// Whether this page decodes a passed pipeline's H.264: a yes needs a decoder that
// gives the page's own stream back a picture for each unit, before the next unit
// is handed over, each in a layout the compositor reads and copied into shared
// memory. Anything else, a browser that throws included, is a no.
import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import {
  chooseRdpH264,
  decodesRdpH264,
  resetRdpH264ForTests,
} from "./rdpH264.ts";

interface Chunk {
  type: string;
  timestamp: number;
  data: Uint8Array;
}

/** What a browser's decoder does with the units it is handed. */
interface Browser {
  /** Whether it has a decoder for the configuration asked about. */
  takes: boolean;
  /**
   * When a unit's picture comes out: as the unit goes in, when the unit after it
   * does, or never.
   */
  gives: "each" | "late" | "none";
  /** How its pictures are laid out. */
  format: string;
  /** Whether a picture copies into shared memory. */
  copies: boolean;
}

const page = globalThis as unknown as {
  VideoDecoder?: unknown;
  EncodedVideoChunk?: unknown;
};
const original = {
  VideoDecoder: page.VideoDecoder,
  EncodedVideoChunk: page.EncodedVideoChunk,
};

/** What the browser was asked, in order. */
let asked: VideoDecoderConfig[] = [];
/** The units handed to a decoder, in order. */
let chunks: Chunk[] = [];
/** The pictures decoders gave, and whether each was closed. */
let frames: { closed: boolean }[] = [];
/** The decoders made, and the state each was left in. */
let decoders: { state: string }[] = [];

/** A browser like `browser`, and the wait a test can afford for a picture. */
function browserWith(browser: Partial<Browser> = {}): void {
  const { takes, gives, format, copies }: Browser = {
    takes: true,
    gives: "each",
    format: "NV12",
    copies: true,
    ...browser,
  };
  resetRdpH264ForTests(20);
  asked = [];
  chunks = [];
  frames = [];
  decoders = [];
  const picture = () => {
    const frame = {
      closed: false,
      format,
      visibleRect: { x: 0, y: 0, width: 1280, height: 800 },
      allocationSize: (options: { rect: { width: number; height: number } }) =>
        (options.rect.width * options.rect.height * 3) / 2,
      copyTo(
        destination: Uint8Array,
        options: { rect: { width: number; height: number } },
      ) {
        assert.ok(destination.buffer instanceof SharedArrayBuffer);
        assert.deepEqual(
          [options.rect.width, options.rect.height, destination.length],
          [16, 16, 384],
        );
        return copies
          ? Promise.resolve([])
          : Promise.reject(new TypeError("not into shared memory"));
      },
      close() {
        this.closed = true;
      },
    };
    frames.push(frame);
    return frame as unknown as VideoFrame;
  };
  page.VideoDecoder = class {
    static isConfigSupported(config: VideoDecoderConfig) {
      asked.push(config);
      return Promise.resolve({ supported: takes });
    }
    state = "unconfigured";
    output: (frame: VideoFrame) => void;
    constructor(init: { output: (frame: VideoFrame) => void }) {
      this.output = init.output;
      decoders.push(this);
    }
    configure() {
      this.state = "configured";
    }
    decode(chunk: Chunk) {
      chunks.push(chunk);
      if (gives === "each" || (gives === "late" && chunks.length > 1)) {
        queueMicrotask(() => this.output(picture()));
      }
    }
    close() {
      this.state = "closed";
    }
  };
  page.EncodedVideoChunk = class {
    type: string;
    timestamp: number;
    data: Uint8Array;
    constructor(init: Chunk) {
      this.type = init.type;
      this.timestamp = init.timestamp;
      this.data = init.data;
    }
  };
}

afterEach(() => {
  page.VideoDecoder = original.VideoDecoder;
  page.EncodedVideoChunk = original.EncodedVideoChunk;
  resetRdpH264ForTests();
});

/** The kinds of the NAL units in an Annex B access unit, in order. */
function nalKinds(unit: Uint8Array): number[] {
  const kinds: number[] = [];
  for (let at = 0; at + 3 < unit.length; at += 1) {
    if (unit[at] === 0 && unit[at + 1] === 0 && unit[at + 2] === 1) {
      kinds.push(unit[at + 3] & 0x1f);
    }
  }
  return kinds;
}

test("a browser whose decoder gives a picture for each unit, copied into shared memory, says yes", async () => {
  browserWith();
  assert.equal(await chooseRdpH264(), true);
  assert.equal(decodesRdpH264(), true);
  // For one picture a unit, and with no say in which decoder the browser uses.
  assert.deepEqual(asked, [
    {
      codec: "avc1.4d4020",
      optimizeForLatency: true,
      colorSpace: {
        primaries: "bt709",
        transfer: "iec61966-2-1",
        matrix: "bt709",
        fullRange: true,
      },
    },
  ]);
  assert.deepEqual(
    chunks.map((chunk) => chunk.type),
    ["key", "delta", "delta"],
  );
  assert.deepEqual(
    frames.map((frame) => frame.closed),
    [true, true, true],
  );
  assert.deepEqual(
    decoders.map((decoder) => decoder.state),
    ["closed"],
  );
  // Asked once: the answer is the page's for as long as it is loaded.
  await chooseRdpH264();
  assert.equal(chunks.length, 3);
});

test("the stream it is asked with is shaped like a host's", async () => {
  browserWith();
  await chooseRdpH264();
  // A delimiter at each unit, the parameter sets inline at the keyframe, and one
  // slice of the kind the unit is.
  assert.deepEqual(
    chunks.map((chunk) => nalKinds(chunk.data)),
    [
      [9, 7, 8, 5],
      [9, 1],
      [9, 1],
    ],
  );
  // The sequence parameter set names the codec string the decoder was asked for:
  // its profile, constraint and level bytes.
  const key = chunks[0].data;
  const sps = key.findIndex(
    (byte, at) => at >= 3 && key[at - 1] === 1 && (byte & 0x1f) === 7,
  );
  assert.equal(
    Array.from(key.subarray(sps + 1, sps + 4), (byte) =>
      byte.toString(16).padStart(2, "0"),
    ).join(""),
    "4d4020",
  );
});

test("a browser with no decoder for the stream says no", async () => {
  browserWith({ takes: false });
  assert.equal(await chooseRdpH264(), false);
  assert.equal(decodesRdpH264(), false);
  assert.deepEqual(chunks, [], "nothing was handed to a decoder it has not");
});

test("nor one whose decoder holds a picture back for the units after it", async () => {
  browserWith({ gives: "late" });
  assert.equal(await chooseRdpH264(), false);
  // The second unit was never handed over: the first gave no picture in time.
  assert.equal(chunks.length, 1);
  assert.deepEqual(
    decoders.map((decoder) => decoder.state),
    ["closed"],
  );

  browserWith({ gives: "none" });
  assert.equal(await chooseRdpH264(), false);
  assert.equal(chunks.length, 1);
});

test("nor one whose pictures the compositor cannot take, or that throws", async () => {
  browserWith({ copies: false });
  assert.equal(await chooseRdpH264(), false);
  assert.deepEqual(
    frames.map((frame) => frame.closed),
    [true],
  );

  browserWith({ format: "BGRX" });
  assert.equal(await chooseRdpH264(), false);
  assert.deepEqual(
    frames.map((frame) => frame.closed),
    [true],
  );

  resetRdpH264ForTests();
  page.VideoDecoder = undefined;
  assert.equal(await chooseRdpH264(), false);
});

test("the answer is not read before it is asked", () => {
  resetRdpH264ForTests();
  assert.throws(() => decodesRdpH264(), /before chooseRdpH264/);
});
