// The batch draw loop, over frames this file builds itself.
//
// Deliberately not built with `protocol.ts`'s encoder — there isn't one, and a
// test that produced its input with the same code that reads it would agree with
// itself no matter what either did. The layout below is transcribed from
// `batch` in `src/protocol.rs`, which is the contract both ends are checked
// against.
//
// Run with `bun test src/framePainter.test.ts` from frontend/.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import type { RelayMessage, RelayPort } from "./displayRelay.ts";
import type { ComposedRun, EgfxCompositor, Scanned } from "./egfxCompositor.ts";
import type { PicturePart } from "./egfxPicture.ts";
import type { EgfxVideo } from "./egfxVideo.ts";
import { createFramePainter, type FramePainter } from "./framePainter.ts";

const OP_VIDEO = 0x03;

interface Unit {
  w: number;
  h: number;
  payload: number[];
  /** Defaults to true: most fixtures below are a stream's first unit. */
  keyframe?: boolean;
}

function batchFrame(units: Unit[]): ArrayBuffer {
  const bytes: number[] = [];
  const u16 = (n: number) => bytes.push(n & 0xff, (n >> 8) & 0xff);
  const u32 = (n: number) =>
    bytes.push(n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >> 24) & 0xff);
  bytes.push(0x02, 0x00);
  u16(units.length);
  u32(1); // attachment-local batch sequence
  for (const unit of units) {
    bytes.push(OP_VIDEO, unit.keyframe === false ? 0 : 0x01);
    u16(unit.w);
    u16(unit.h);
    u32(unit.payload.length);
    bytes.push(...unit.payload);
  }
  return new Uint8Array(bytes).buffer;
}

// An access unit's payload. **Opaque here, deliberately**: nothing on this side of the
// wire parses a bitstream — the gateway says how to decode the stream in a `videoFormat`
// message and marks each unit's keyframe bit in the record — so these bytes only need to
// be distinguishable from each other.
const KEYFRAME = [0x82, 0x49, 0x83, 0x42, 0x00, 0x13, 0xf0];

interface FakeFrame {
  closed: boolean;
}

/** Nine-argument draws: the source rectangle is what crops a padded frame. */
let cropped: {
  sx: number;
  sy: number;
  sw: number;
  sh: number;
  dx: number;
  dy: number;
  dw: number;
  dh: number;
}[] = [];
let decoded: FakeFrame[] = [];
let videoErrors: (string | null)[] = [];
/** Chains that were cut. The stub never goes quiet, so these are failures. */
let videoKeyframeAsks: string[] = [];

const context = {
  drawImage(_source: unknown, ...args: number[]) {
    cropped.push({
      sx: args[0],
      sy: args[1],
      sw: args[2],
      sh: args[3],
      dx: args[4],
      dy: args[5],
      dw: args[6],
      dh: args[7],
    });
  },
} as unknown as CanvasRenderingContext2D;

// The WebCodecs decoder, which this runtime has none of — and which the client
// refuses to start without, so it is installed for every test here. Only its shape
// matters: what the painter does with the frames, not what a real decoder makes of
// the bitstream — that is browser QA.
let chunkTypes: string[] = [];
/** How many decoders were built — replaced on a resize. */
let decoders = 0;
let closes = 0;
/** A payload whose last byte is this makes its decoder give up. */
let poison: number | null = null;
/** Like `poison`, but the browser refusing the configuration rather than failing. */
let refused: number | null = null;
/** Like `poison`, but `decode()` itself throwing rather than the error callback. */
let rejected: number | null = null;
/** The configurations the decoders were built with. */
let configured: string[] = [];
/** Whether a decoder outputs planes, as the software HEVC decoder does. */
let planar = false;

class FakeVideoDecoder {
  private readonly output: (frame: unknown) => void;
  private readonly fail: (error: Error) => void;
  state = "unconfigured";

  constructor(init: {
    output: (frame: unknown) => void;
    error: (error: Error) => void;
  }) {
    this.output = init.output;
    this.fail = init.error;
    decoders += 1;
  }

  configure(config: { codec: string }) {
    configured.push(config.codec);
    this.state = "configured";
  }

  decode(chunk: { type: string; data?: Uint8Array }) {
    const last = chunk.data?.[chunk.data.length - 1];
    if (rejected !== null && last === rejected) {
      throw new TypeError("this chunk is not acceptable");
    }
    if (poison !== null && last === poison) {
      this.fail(new Error("this decoder gave up"));
      return;
    }
    if (refused !== null && last === refused) {
      // The name is the whole signal: it is how WebCodecs says "not this
      // configuration", which no later keyframe changes.
      const no = new Error("this configuration is not supported");
      no.name = "NotSupportedError";
      this.fail(no);
      return;
    }
    chunkTypes.push(chunk.type);
    const frame: FakeFrame = { closed: false };
    decoded.push(frame);
    this.output({
      // What marks the software HEVC decoder's picture from a `VideoFrame`.
      ...(planar ? { planes: [] } : {}),
      close() {
        frame.closed = true;
      },
    });
  }

  close() {
    closes += 1;
    this.state = "closed";
  }
}

const globals = globalThis as unknown as {
  VideoDecoder: unknown;
  EncodedVideoChunk: unknown;
};

beforeEach(() => {
  uploaded = [];
  pictures = { made: 0, closed: 0 };
  blanked = [];
  windows = [];
  patched = [];
  shown = [];
  cropped = [];
  decoded = [];
  videoErrors = [];
  videoKeyframeAsks = [];
  chunkTypes = [];
  decoders = 0;
  closes = 0;
  poison = null;
  refused = null;
  rejected = null;
  configured = [];
  planar = false;
  hevc = { made: 0, closed: 0, drawn: [] };
  globals.VideoDecoder = FakeVideoDecoder;
  globals.EncodedVideoChunk = class {
    type: string;
    data: Uint8Array;
    constructor(init: { type: string; data: Uint8Array }) {
      this.type = init.type;
      this.data = init.data;
    }
  };
});

afterEach(() => {
  globals.VideoDecoder = undefined;
  globals.EncodedVideoChunk = undefined;
});

function painter(ctx: CanvasRenderingContext2D | null = context) {
  return createFramePainter({
    context: () => ctx,
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
  });
}

// The `videoFormat` a gateway sends before the stream's first unit. Every test goes
// through this, because a painter with no format refuses to decode — and that refusal
// has a test of its own below.
function announced(
  ctx: CanvasRenderingContext2D | null = context,
): FramePainter {
  const p = painter(ctx);
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  return p;
}

test("a malformed frame is dropped whole", async () => {
  // Half an access unit is not a smaller access unit: submitting one would leave
  // the decoder's state wrong for every frame after it.
  const frame = batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]);
  await announced().draw(frame.slice(0, frame.byteLength - 1));
  assert.deepEqual(cropped, []);
  assert.deepEqual(chunkTypes, []);
});

test("a frame is cropped to the desktop, drawn at the origin", async () => {
  // The encoder is held to even sides and an odd desktop does not have them, so the
  // decoded picture can be a pixel wider or taller than the desktop.
  await announced().draw(batchFrame([{ w: 1599, h: 1015, payload: KEYFRAME }]));
  assert.deepEqual(cropped, [
    { sx: 0, sy: 0, sw: 1599, sh: 1015, dx: 0, dy: 0, dw: 1599, dh: 1015 },
  ]);
  assert.deepEqual(chunkTypes, ["key"]);
  assert.ok(
    decoded.every((frame) => frame.closed),
    "a VideoFrame holds decoder memory until it is closed",
  );
});

test("a record's keyframe flag decides the chunk type, both ways", async () => {
  // The flag comes from the encoder, and VP9 — which has no parameter sets — offers a
  // client nothing to work it out from.
  await announced().draw(
    batchFrame([
      { w: 64, h: 64, payload: KEYFRAME },
      { w: 64, h: 64, payload: [1, 2, 3], keyframe: false },
    ]),
  );
  assert.deepEqual(chunkTypes, ["key", "delta"]);
});

test("a video record with an unknown flag drops the batch", async () => {
  // The same strictness the frame's own flags byte gets: a bit this client does not
  // know means a gateway newer than it, and painting half of what it meant is worse
  // than painting none of it.
  const frame = batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]);
  // Byte 8 is the op, 9 the flags.
  new Uint8Array(frame)[9] = 0x02;
  await announced().draw(frame);
  assert.deepEqual(chunkTypes, []);
  assert.deepEqual(cropped, []);
});

test("a record of an unknown op drops the batch", async () => {
  const frame = batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]);
  new Uint8Array(frame)[8] = 0x02;
  await announced().draw(frame);
  assert.deepEqual(chunkTypes, []);
});

test("units that arrive before their format are dropped, not reported", async () => {
  // The reattach: the gateway announces the stream once, to whoever was attached, so a
  // page that comes back to the session gets whatever was already in flight before the
  // repaint its attach triggers. Those units cannot be decoded here whatever happens,
  // so they are dropped in silence, and the repaint that follows carries the format
  // and a keyframe.
  const p = painter();
  await p.draw(
    batchFrame([{ w: 64, h: 64, payload: [4, 5, 6], keyframe: false }]),
  );
  assert.deepEqual(
    videoErrors,
    [null],
    "a unit before its format was reported",
  );
  assert.deepEqual(chunkTypes, []);
  assert.deepEqual(cropped, []);

  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.deepEqual(
    chunkTypes,
    ["key"],
    "the stream did not recover once announced",
  );
  assert.equal(cropped.length, 1);
});

test("a stream that restarts on a new size replaces its decoder", async () => {
  // The configuration string carries no resolution, so an in-band size change is not
  // something to bet two browsers on.
  const p = announced();
  await p.draw(batchFrame([{ w: 320, h: 64, payload: KEYFRAME }]));
  await p.draw(batchFrame([{ w: 640, h: 64, payload: KEYFRAME }]));
  assert.equal(
    decoders,
    2,
    "the resized desktop kept a decoder built for the old size",
  );
  assert.equal(closes, 1, "the replaced decoder was left holding memory");
});

test("a failed decoder asks for a keyframe, and the complaint goes when video paints again", async () => {
  poison = 0xbd;
  const p = announced();
  await p.draw(batchFrame([{ w: 320, h: 64, payload: [...KEYFRAME, 0xbd] }]));
  assert.equal(
    videoErrors.at(-1),
    "This browser's video decoder failed (Error: this decoder gave up).",
  );
  assert.equal(
    videoKeyframeAsks.length,
    1,
    "a failed decoder is thrown away, and only a keyframe starts another",
  );

  poison = null;
  await p.draw(batchFrame([{ w: 320, h: 64, payload: KEYFRAME }]));
  assert.equal(
    videoErrors.at(-1),
    null,
    "the banner outlived what it described",
  );
});

test("a refused stream says so, asks for nothing, and stays said", async () => {
  // The whole reason this is reported at all: the stream is all a target sends, so the
  // alternative is a desktop that never paints and never explains itself.
  refused = 0xbd;
  const p = announced();
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [...KEYFRAME, 0xbd] }]));
  const said = videoErrors.filter(Boolean);
  assert.equal(said.length, 1);
  assert.match(String(said[0]), /cannot decode/);
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "a keyframe was asked for on a configuration no keyframe repairs",
  );
  assert.deepEqual(cropped, []);
});

test("clear() retracts the complaint and ends the decoder", async () => {
  // The attachment boundary. The page clears its own copy on the way back to the
  // picker only, so a reattach would otherwise inherit this sentence.
  refused = 0xbd;
  const p = announced();
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [...KEYFRAME, 0xbd] }]));
  assert.notEqual(videoErrors.at(-1), null);
  p.clear();
  assert.equal(videoErrors.at(-1), null);

  refused = null;
  const q = announced();
  await q.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  q.clear();
  assert.ok(closes >= 1, "the decoder belongs to one attachment");
});

test("every decoded frame is closed, even with nowhere to draw it", async () => {
  await announced(null).draw(
    batchFrame([
      { w: 64, h: 64, payload: KEYFRAME },
      { w: 64, h: 64, payload: [1], keyframe: false },
    ]),
  );
  assert.deepEqual(cropped, []);
  assert.equal(decoded.length, 2);
  assert.ok(
    decoded.every((frame) => frame.closed),
    "a frame that is never drawn still has to be released",
  );
});

test("a malformed batch cuts the chain: the decoder restarts and a keyframe is asked for", async () => {
  // Every unit is part of one chain, so the deltas after a dropped batch name a picture
  // this decoder never made.
  const p = announced();
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  const frame = batchFrame([{ w: 64, h: 64, payload: [1], keyframe: false }]);
  await p.draw(frame.slice(0, frame.byteLength - 1));
  assert.equal(videoKeyframeAsks.length, 1);
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [2], keyframe: false }]));
  assert.deepEqual(chunkTypes, ["key"], "a delta was fed past the cut");
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.deepEqual(chunkTypes, ["key", "key"]);
  assert.equal(decoders, 2, "the cut chain kept its decoder");
});

test("a unit decode() throws on cuts the chain without misplacing a frame", async () => {
  // The unit was never consumed, so every delta after it is against a missing picture.
  rejected = 0xbd;
  const p = announced();
  await p.draw(
    batchFrame([
      { w: 64, h: 64, payload: KEYFRAME },
      { w: 64, h: 64, payload: [1, 0xbd], keyframe: false },
      { w: 64, h: 64, payload: [2], keyframe: false },
    ]),
  );
  assert.equal(videoKeyframeAsks.length, 1, "no keyframe was asked for");
  // Raised, then taken down again by the keyframe decoded ahead of the cut, which
  // still paints.
  assert.ok(
    videoErrors.some((error) => error?.includes("refused a frame")),
    String(videoErrors),
  );
  assert.deepEqual(chunkTypes, ["key"], "a delta was fed past the cut");
  assert.ok(decoded.every((frame) => frame.closed));

  rejected = null;
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.equal(videoErrors.at(-1), null, "the stream did not recover");
});

test("a refusal gives way to a configuration the browser takes", async () => {
  // The announced VP9 level follows the desktop's size, so a browser that refused a
  // large picture may decode the smaller one a resize brings.
  refused = 0xbd;
  const p = painter();
  p.setVideoFormat({ decode: "vp09.01.51.08" });
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [...KEYFRAME, 0xbd] }]));
  assert.match(String(videoErrors.at(-1)), /cannot decode/);

  // The same configuration again changes nothing.
  p.setVideoFormat({ decode: "vp09.01.51.08" });
  await p.draw(batchFrame([{ w: 64, h: 64, payload: [3], keyframe: false }]));
  assert.match(String(videoErrors.at(-1)), /cannot decode/);

  refused = null;
  p.setVideoFormat({ decode: "vp09.01.40.08" });
  assert.match(
    String(videoErrors.at(-1)),
    /cannot decode/,
    "the banner came down before anything painted",
  );
  await p.draw(batchFrame([{ w: 32, h: 32, payload: KEYFRAME }]));
  assert.equal(cropped.length, 1);
  assert.equal(
    videoErrors.at(-1),
    null,
    "the refusal outlived its configuration",
  );
  assert.equal(configured.at(-1), "vp09.01.40.08");
});

// A GRAPHICS record: op 0x04, a length, the commands.
function graphicsFrame(runs: number[][]): ArrayBuffer {
  const bytes: number[] = [0x02, 0x00, runs.length & 0xff, runs.length >> 8];
  bytes.push(1, 0, 0, 0);
  for (const run of runs) {
    const n = run.length;
    bytes.push(0x04, n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, n >>> 24);
    bytes.push(...run);
  }
  return new Uint8Array(bytes).buffer;
}

// A compositor that records what it was fed and paints what it is told to. What a
// real one makes of the commands is egfxCompositor.test.ts; here it is the painter's
// handling of one that is under test.
//
// A run whose first byte is `H264` carries H.264: its second byte is how many
// access units, each one byte long, and its third a surface whose stream ended.
const H264 = 200;
function h264Runs(commands: Uint8Array): Scanned[] {
  if (commands[0] !== H264) {
    return [];
  }
  const found: Scanned[] = [];
  if (commands[2] !== undefined) {
    found.push({ gone: commands[2] });
  }
  for (let number = 0; number < commands[1]; number += 1) {
    const start = 3 + number;
    found.push({
      unit: {
        surface: 1,
        start,
        end: start + 1,
        key: number === 0,
        codec: number === 0 ? "avc1.4d4020" : null,
        window: "whole",
      },
      number,
    });
  }
  return found;
}

function fakeCompositors(options: { refuse?: number; fail?: boolean } = {}) {
  const made: {
    fed: number[][];
    closed: boolean;
    /** Each picture supplied: its number in its run, and the unit's byte. */
    supplied: number[][];
  }[] = [];
  const load = () => {
    if (options.fail) {
      return Promise.reject(new Error("no module"));
    }
    return Promise.resolve((): EgfxCompositor => {
      const record = {
        fed: [] as number[][],
        closed: false,
        supplied: [] as number[][],
      };
      made.push(record);
      // The picture: 64 by 48, every byte the last run's first byte, and of
      // nothing before the first run, as a compositor's is before its reset.
      const pixels = new Uint8ClampedArray(64 * 48 * 4);
      const picture = () =>
        record.fed.length === 0
          ? { width: 0, height: 0, pixels: new Uint8ClampedArray(0) }
          : { width: 64, height: 48, pixels };
      return {
        scan: h264Runs,
        supply(number, _window, frame) {
          assert.equal(
            record.closed,
            false,
            "a closed compositor was supplied",
          );
          record.supplied.push([
            number,
            (frame as unknown as FakePicture).unit,
          ]);
          return Promise.resolve();
        },
        compose(commands): ComposedRun {
          if (commands[0] === options.refuse) {
            throw new Error("a command that does not decode");
          }
          record.fed.push([...commands]);
          pixels.fill(commands[0]);
          return {
            // Each run paints one rectangle named by its first byte; the first
            // run of a pipeline is the reset that lays the picture out.
            painted: new Uint32Array([commands[0], 2, 3, 4]),
            resized: record.fed.length === 1,
            ...picture(),
          };
        },
        picture,
        close() {
          record.closed = true;
        },
      };
    });
  };
  return { made, load };
}

/** A decoded picture: the byte of the unit it came from, and whether it is closed. */
interface FakePicture {
  unit: number;
  closed: boolean;
  close(): void;
}

/**
 * A pipeline's H.264 decoders, recording what they were asked. `hold` keeps each
 * decode waiting until `release` is called, and `fail` rejects every one.
 */
function fakeVideo(options: { fail?: boolean; hold?: boolean } = {}) {
  const log: string[] = [];
  const frames: FakePicture[] = [];
  const held: (() => void)[] = [];
  let made = 0;
  const make = (): EgfxVideo => {
    made += 1;
    let closed = false;
    return {
      decode(unit, data) {
        log.push(`decode ${unit.surface}:${data[0]}${unit.key ? " key" : ""}`);
        return new Promise((resolve, reject) => {
          const settle = () => {
            if (options.fail || closed) {
              reject(new Error("its H.264 decoder failed: nothing"));
              return;
            }
            const frame: FakePicture = {
              unit: data[0],
              closed: false,
              close() {
                this.closed = true;
              },
            };
            frames.push(frame);
            resolve(frame as unknown as VideoFrame);
          };
          if (options.hold) {
            held.push(settle);
          } else {
            settle();
          }
        });
      },
      drop(surface) {
        log.push(`drop ${surface}`);
      },
      close() {
        closed = true;
        log.push("close");
        for (const settle of held.splice(0)) {
          settle();
        }
      },
    };
  };
  return { make, log, frames, held, made: () => made };
}

/** The picture of a pipeline: what each run's upload named, painted or not. */
let uploaded: number[][] = [];
/** How many pictures were made, and how many closed. */
let pictures = { made: 0, closed: 0 };
/** The sizes a picture was blanked at. */
let blanked: number[][] = [];
/** The parts a picture was told to show, in order. */
let windows: (PicturePart | null)[] = [];
/** What a picture was patched with: its size, the rectangles, the bytes. */
let patched: [number, number, number[], number][] = [];
/** What the page was told about showing the picture, in order. */
let shown: boolean[] = [];

/** One end of the second display's channel, by hand: what it posted, and a way to
 * deliver what the other end says. */
function fakeRelay() {
  const posted: RelayMessage[] = [];
  let handler: (message: RelayMessage) => void = () => {};
  let closed = 0;
  const port: RelayPort = {
    post: (message) => {
      posted.push(message);
    },
    onMessage: (next) => {
      handler = next;
    },
    close: () => {
      closed += 1;
    },
  };
  return {
    port,
    posted,
    deliver: (message: RelayMessage) => handler(message),
    closed: () => closed,
  };
}

function graphicsPainter(
  load: ReturnType<typeof fakeCompositors>["load"],
  options: {
    noPicture?: boolean;
    blankFails?: boolean;
    video?: () => EgfxVideo;
    relay?: RelayPort;
  } = {},
) {
  return createFramePainter({
    makeRelay: () => options.relay ?? fakeRelay().port,
    makeGraphicsVideo:
      options.video ??
      (() => {
        throw new Error("a pipeline without H.264 made a decoder for it");
      }),
    context: () => context,
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
    loadCompositor: load,
    makePicture: () => {
      if (options.noPicture) {
        throw new Error("WebGL 2 is not available");
      }
      pictures.made += 1;
      return {
        upload(run) {
          uploaded.push([...run.painted]);
        },
        window(part) {
          windows.push(part);
        },
        patch(w, h, rects, pixels) {
          patched.push([w, h, Array.from(rects), pixels.length]);
        },
        blank(w, h) {
          if (options.blankFails) {
            throw new Error("the GPU refused the picture");
          }
          blanked.push([w, h]);
        },
        close() {
          pictures.closed += 1;
        },
      };
    },
    onGraphicsShown: (on) => {
      shown.push(on);
    },
  });
}

test("a pipeline's runs are composed in order and their rectangles uploaded", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[7, 1], [9]]));
  await p.draw(graphicsFrame([[11, 5, 5]]));
  assert.equal(made.length, 1, "one compositor follows the whole pipeline");
  assert.deepEqual(made[0].fed, [[7, 1], [9], [11, 5, 5]]);
  assert.deepEqual(uploaded, [
    [7, 2, 3, 4],
    [9, 2, 3, 4],
    [11, 2, 3, 4],
  ]);
  // The picture is shown where it is drawn: nothing of it goes onto the
  // desktop's canvas, and the page is told to show it once, at the first run.
  assert.deepEqual(cropped, []);
  assert.deepEqual(shown, [true]);
  assert.equal(decoders, 0, "a pipeline built a video decoder");
  assert.deepEqual(videoKeyframeAsks, []);
});

test("a pipeline that starts again is composed from nothing", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  p.startGraphics();
  await p.draw(graphicsFrame([[2]]));
  assert.deepEqual(
    made.map((compositor) => [compositor.fed, compositor.closed]),
    [
      [[[1]], true],
      [[[2]], false],
    ],
  );
  assert.deepEqual(
    pictures,
    { made: 2, closed: 1 },
    "a picture each, the first given back",
  );
  assert.deepEqual(
    shown,
    [true, false, true],
    "the first hidden before the second has drawn anything",
  );
});

test("a picture is not shown before its pipeline has drawn a run", async () => {
  const { load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([]));
  assert.deepEqual(shown, []);
  p.clear();
  assert.deepEqual(shown, [], "and one never shown is not hidden");
});

test("a desktop resized under a pipeline blanks its picture", async () => {
  const { load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.blank(800, 600);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  p.blank(1024, 768);
  assert.deepEqual(blanked, [[1024, 768]], "only a pipeline's picture");
  assert.deepEqual(
    videoErrors.filter((error) => error !== null),
    [],
  );
});

test("the part of the picture a display is, is what the picture shows", async () => {
  const { load } = fakeCompositors();
  const p = graphicsPainter(load);
  // Named ahead of the pipeline, as the gateway's `resize` and `graphicsView`
  // come ahead of its `graphicsStart`: applied to the picture when it is made.
  p.setGraphicsView({ x: 32, y: 0, w: 32, h: 48 });
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  assert.deepEqual(windows, [{ x: 32, y: 0, w: 32, h: 48 }]);
  // The picker moving to the other display: shown at once, from what the
  // picture holds, and kept by the next pipeline.
  p.setGraphicsView({ x: 0, y: 0, w: 32, h: 48 });
  p.startGraphics();
  await p.draw(graphicsFrame([[2]]));
  assert.deepEqual(windows, [
    { x: 32, y: 0, w: 32, h: 48 },
    { x: 0, y: 0, w: 32, h: 48 },
    { x: 0, y: 0, w: 32, h: 48 },
  ]);
  // The attachment boundary forgets it: the next names its own.
  p.clear();
  p.startGraphics();
  await p.draw(graphicsFrame([[3]]));
  assert.equal(windows[windows.length - 1], null);
});

test("a tab showing the second display is sent its column of the picture", async () => {
  const { load } = fakeCompositors();
  const relay = fakeRelay();
  const p = graphicsPainter(load, { relay: relay.port });
  p.startGraphics();
  assert.deepEqual(
    relay.posted,
    [{ kind: "composing" }],
    "a source that starts asks",
  );
  // The tab answers before the first run: it is owed everything once there is a
  // picture, which the first run — the reset — lays out.
  relay.deliver({
    kind: "shown",
    display: 2,
    part: { x: 32, y: 0, w: 32, h: 48 },
  });
  await p.draw(graphicsFrame([[40]]));
  assert.equal(relay.posted.length, 2);
  const first = relay.posted[1];
  assert.equal(first.kind, "paint");
  if (first.kind !== "paint") {
    return;
  }
  assert.deepEqual(
    [first.seq, first.w, first.h, first.rects],
    [1, 32, 48, [0, 0, 32, 48]],
  );
  const bytes = new Uint8Array(first.pixels);
  assert.equal(bytes.length, 32 * 48 * 4);
  assert.ok(
    bytes.every((byte) => byte === 40),
    "the column, out of the picture",
  );
  // While that is in flight, what the runs paint waits and is merged; what
  // falls outside the column is nothing to the tab.
  await p.draw(graphicsFrame([[36]]));
  await p.draw(graphicsFrame([[50]]));
  await p.draw(graphicsFrame([[7]]));
  assert.equal(relay.posted.length, 2, "one update in flight at a time");
  relay.deliver({ kind: "painted", seq: 1 });
  const second = relay.posted[2];
  assert.equal(second?.kind, "paint");
  if (second?.kind !== "paint") {
    return;
  }
  // Relative to the column, and out of the picture as it stands now.
  assert.deepEqual([second.seq, second.rects], [2, [4, 2, 3, 4, 18, 2, 3, 4]]);
  const latest = new Uint8Array(second.pixels);
  assert.equal(latest.length, 2 * 3 * 4 * 4);
  assert.ok(latest.every((byte) => byte === 7));
  // A pipeline that starts again owes nothing of the old picture; its first run
  // owes the column again.
  relay.deliver({ kind: "painted", seq: 2 });
  p.startGraphics();
  await p.draw(graphicsFrame([[9]]));
  const third = relay.posted[3];
  assert.equal(third?.kind, "paint");
  if (third?.kind === "paint") {
    assert.deepEqual(third.rects, [0, 0, 32, 48]);
  }
});

test("a tab is painted from the session page's picture, not composed", async () => {
  const { made, load } = fakeCompositors();
  const relay = fakeRelay();
  const p = graphicsPainter(load, { relay: relay.port });
  p.blank(32, 48);
  p.mirrorGraphics(2, { x: 32, y: 0, w: 32, h: 48 });
  assert.deepEqual(relay.posted, [
    { kind: "shown", display: 2, part: { x: 32, y: 0, w: 32, h: 48 } },
  ]);
  assert.deepEqual(shown, [], "nothing to show until the first update");
  relay.deliver({
    kind: "paint",
    seq: 3,
    w: 32,
    h: 48,
    rects: [0, 0, 32, 48],
    pixels: new ArrayBuffer(32 * 48 * 4),
  });
  assert.deepEqual(patched, [[32, 48, [0, 0, 32, 48], 32 * 48 * 4]]);
  assert.deepEqual(relay.posted[1], { kind: "painted", seq: 3 });
  assert.deepEqual(shown, [true]);
  // A session page that starts composing asks; the tab says again what it shows.
  relay.deliver({ kind: "composing" });
  assert.deepEqual(relay.posted[2], {
    kind: "shown",
    display: 2,
    part: { x: 32, y: 0, w: 32, h: 48 },
  });
  // A layout change: the tab's desktop blanks its picture, and the new part is
  // said on the same channel.
  p.blank(40, 48);
  p.mirrorGraphics(2, { x: 32, y: 0, w: 40, h: 48 });
  assert.deepEqual(blanked, [[40, 48]]);
  assert.equal(relay.posted.length, 4);
  assert.equal(made.length, 0, "a tab composes nothing");
  // The attachment boundary gives the picture back and hides it.
  p.clear();
  assert.deepEqual(shown, [true, false]);
  assert.deepEqual(pictures, { made: 1, closed: 1 });
  assert.equal(relay.closed(), 1);
});

test("a video format takes the display back from a tab's mirror", async () => {
  // The session stops passing (a host that draws with bitmap updates after all):
  // the tab is sent a stream, which is drawn on the desktop's canvas the mirror's
  // picture covered.
  const { load } = fakeCompositors();
  const relay = fakeRelay();
  const p = graphicsPainter(load, { relay: relay.port });
  p.mirrorGraphics(2, { x: 32, y: 0, w: 32, h: 48 });
  relay.deliver({
    kind: "paint",
    seq: 1,
    w: 32,
    h: 48,
    rects: [0, 0, 32, 48],
    pixels: new ArrayBuffer(32 * 48 * 4),
  });
  assert.deepEqual(shown, [true]);
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.deepEqual(chunkTypes, ["key"]);
  assert.deepEqual(shown, [true, false]);
  assert.deepEqual(pictures, { made: 1, closed: 1 });
  assert.equal(relay.closed(), 1);
  // A display passed again is mirrored afresh.
  p.mirrorGraphics(2, { x: 32, y: 0, w: 32, h: 48 });
  assert.equal(pictures.made, 2);
});

test("a picture that cannot be blanked ends its pipeline", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load, { blankFails: true });
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  p.blank(1024, 768);
  await p.draw(graphicsFrame([[2]]));
  assert.deepEqual(made[0].fed, [[1]]);
  const said = videoErrors.filter((error) => error !== null);
  assert.match(said[0] ?? "", /the GPU refused the picture/);
});

test("a run's H.264 is decoded and supplied before the run is composed", async () => {
  const { made, load } = fakeCompositors();
  const video = fakeVideo();
  const p = graphicsPainter(load, { video: video.make });
  p.startGraphics();
  await p.draw(graphicsFrame([[1], [H264, 2, 5, 31, 32], [H264, 1, 5, 33]]));
  assert.equal(video.made(), 1, "one set of decoders follows the pipeline");
  // Each unit goes through in command order, the first of a stream as its key,
  // and a surface's stream is ended before the units after it are decoded — once
  // there are decoders for one to have been in.
  assert.deepEqual(video.log, [
    "decode 1:31 key",
    "decode 1:32",
    "drop 5",
    "decode 1:33 key",
  ]);
  assert.deepEqual(
    made[0].supplied,
    [
      [0, 31],
      [1, 32],
      [0, 33],
    ],
    "each picture under its unit's number in its own run",
  );
  assert.deepEqual(
    made[0].fed.map((run) => run[0]),
    [1, H264, H264],
  );
  assert.ok(
    video.frames.every((frame) => frame.closed),
    "a picture handed over is closed",
  );
  assert.deepEqual(
    videoErrors.filter((error) => error !== null),
    [],
  );
  p.clear();
  assert.equal(video.log.at(-1), "close");
});

test("an H.264 unit that gives no picture ends the pipeline and says so", async () => {
  const { made, load } = fakeCompositors();
  const video = fakeVideo({ fail: true });
  const p = graphicsPainter(load, { video: video.make });
  p.startGraphics();
  await p.draw(graphicsFrame([[1], [H264, 1, 5, 31], [3]]));
  assert.deepEqual(made[0].fed, [[1]], "the run was not composed without it");
  assert.equal(made[0].closed, true);
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1);
  assert.match(said[0] ?? "", /could not compose the host's graphics/);
  assert.match(said[0] ?? "", /H\.264 decoder failed/);
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "a host sends no keyframe on request",
  );
});

test("an attachment that ends while a unit decodes leaves its compositor alone", async () => {
  const { made, load } = fakeCompositors();
  const video = fakeVideo({ hold: true });
  const p = graphicsPainter(load, { video: video.make });
  p.startGraphics();
  const late = p.draw(graphicsFrame([[H264, 1, 5, 31]]));
  while (video.held.length === 0) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
  // The clear closes the decoders, which settles the unit the run waits on.
  p.clear();
  await late;
  assert.equal(made[0].closed, true);
  assert.deepEqual(made[0].supplied, []);
  assert.deepEqual(made[0].fed, []);
  assert.deepEqual(
    videoErrors.filter((error) => error !== null),
    [],
    "an ended attachment has nothing to say",
  );
});

test("a run with no pipeline started is dropped", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  await p.draw(graphicsFrame([[1]]));
  assert.deepEqual(made, []);
  assert.deepEqual(uploaded, []);
});

test("a command that does not decode ends the pipeline and says so", async () => {
  const { made, load } = fakeCompositors({ refuse: 66 });
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1], [66], [3]]));
  await p.draw(graphicsFrame([[4]]));
  assert.deepEqual(made[0].fed, [[1]], "nothing is composed after the refusal");
  assert.equal(made[0].closed, true);
  assert.equal(uploaded.length, 1);
  assert.deepEqual(
    [pictures.closed, shown],
    [0, [true]],
    "the picture stays, as the desktop under the sentence",
  );
  p.clear();
  assert.deepEqual([pictures.closed, shown], [1, [true, false]]);
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1);
  assert.match(said[0] ?? "", /could not compose the host's graphics/);
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "no repaint repairs a pipeline: the host answers one out of its caches",
  );
});

test("a browser without WebGL 2 is told the compositor could not be loaded", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load, { noPicture: true });
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  const said = videoErrors.filter((error) => error !== null);
  assert.match(said[0] ?? "", /could not load the graphics compositor/);
  assert.match(said[0] ?? "", /WebGL 2 is not available/);
  assert.equal(made[0]?.closed, true, "the compositor made is given back");
  assert.deepEqual(uploaded, []);
});

test("a compositor that will not load says so, and the next pipeline tries again", async () => {
  const failing = fakeCompositors({ fail: true });
  const p = graphicsPainter(failing.load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  const said = videoErrors.filter((error) => error !== null);
  assert.match(said[0] ?? "", /could not load the graphics compositor/);
  assert.deepEqual(uploaded, []);
});

test("a video format takes the picture back from a pipeline", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(graphicsFrame([[2]]));
  await p.draw(batchFrame([{ w: 64, h: 64, payload: KEYFRAME }]));
  assert.deepEqual(made[0].fed, [[1]]);
  assert.equal(made[0].closed, true);
  assert.deepEqual(chunkTypes, ["key"]);
  assert.deepEqual(
    shown,
    [true, false],
    "the stream is drawn on the desktop's canvas, which the picture covered",
  );
});

test("clear() ends a pipeline, and a run of the attachment before is not painted", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  const late = p.draw(graphicsFrame([[1]]));
  p.clear();
  await late;
  assert.deepEqual(uploaded, []);
  assert.deepEqual(made, [], "the module loaded for a pipeline that was gone");
});

test("a malformed batch ends a pipeline: what it held is never composed, and no repaint is asked for", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[1]]));
  // A record of no length is malformed, and the batch is dropped whole.
  await p.draw(graphicsFrame([[2], []]));
  await p.draw(graphicsFrame([[3]]));
  await p.draw(graphicsFrame([[]]));
  assert.deepEqual(
    made[0].fed,
    [[1]],
    "nothing is composed after the dropped batch",
  );
  assert.equal(made[0].closed, true);
  assert.equal(uploaded.length, 1);
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1, "the end of a pipeline is said once");
  assert.match(said[0] ?? "", /could not compose the host's graphics/);
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "no repaint repairs a pipeline: the host answers one out of its caches",
  );
});

test("a malformed batch ahead of the module's load leaves no compositor made", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[]]));
  await p.draw(graphicsFrame([[1]]));
  assert.deepEqual(made, []);
  assert.deepEqual(uploaded, []);
  assert.deepEqual(videoKeyframeAsks, []);
});

test("a pipeline that starts after one ended is composed", async () => {
  const { made, load } = fakeCompositors();
  const p = graphicsPainter(load);
  p.startGraphics();
  await p.draw(graphicsFrame([[]]));
  p.startGraphics();
  await p.draw(graphicsFrame([[5]]));
  assert.deepEqual(
    made.map((compositor) => compositor.fed),
    [[[5]]],
  );
  assert.equal(videoErrors.at(-1), null, "the next pipeline retracts it");
});

/** The software HEVC decoder's picture: how many were made and closed, and the sizes drawn. */
let hevc: { made: number; closed: number; drawn: number[][] } = {
  made: 0,
  closed: 0,
  drawn: [],
};

const HEVC = { decode: "hev1.4.10.L150.BE.8" };

function hevcPainter(options: { refuses?: boolean } = {}) {
  planar = true;
  const p = createFramePainter({
    context: () => context,
    onVideoError: (error) => {
      videoErrors.push(error);
    },
    onVideoNeedsKeyframe: (reason) => {
      videoKeyframeAsks.push(reason);
    },
    makeHevcPicture: () => {
      hevc.made += 1;
      return {
        draw(_planes, w, h) {
          if (options.refuses) {
            throw new Error("the GPU refused the picture");
          }
          hevc.drawn.push([w, h]);
        },
        close() {
          hevc.closed += 1;
        },
      };
    },
    onGraphicsShown: (on) => {
      shown.push(on);
    },
  });
  p.setVideoFormat(HEVC);
  return p;
}

test("the software decoder's pictures are drawn over the desktop's canvas, and closed", async () => {
  const p = hevcPainter();
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  await p.draw(batchFrame([{ w: 640, h: 480, payload: [1], keyframe: false }]));
  assert.deepEqual(hevc.drawn, [
    [640, 480],
    [640, 480],
  ]);
  assert.equal(hevc.made, 1, "one picture follows the stream");
  assert.deepEqual(cropped, [], "nothing of it goes onto the desktop's canvas");
  assert.deepEqual(shown, [true], "shown once, at the first picture");
  // The decoder starts its next unit only once a picture is closed.
  assert.deepEqual(
    decoded.map((frame) => frame.closed),
    [true, true],
  );
});

test("a stream encoded here takes the picture back from the software decoder's", async () => {
  const p = hevcPainter();
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  // The Mac's stream gives way across a display change, and comes back.
  planar = false;
  p.setVideoFormat({ decode: "vp09.00.40.08" });
  await p.draw(batchFrame([{ w: 800, h: 600, payload: KEYFRAME }]));
  assert.equal(cropped.length, 1);
  assert.deepEqual(shown, [true, false]);
  planar = true;
  p.setVideoFormat(HEVC);
  await p.draw(batchFrame([{ w: 800, h: 600, payload: KEYFRAME }]));
  assert.deepEqual(shown, [true, false, true]);
  assert.deepEqual(hevc.drawn, [
    [640, 480],
    [800, 600],
  ]);
  assert.deepEqual([hevc.made, hevc.closed], [1, 0], "kept across the change");
});

test("a resize hides the software decoder's picture until its next one", async () => {
  const p = hevcPainter();
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  p.blank(800, 600);
  assert.deepEqual(shown, [true, false]);
  await p.draw(batchFrame([{ w: 800, h: 600, payload: KEYFRAME }]));
  assert.deepEqual(shown, [true, false, true]);
});

test("a picture the GPU refuses is said once, and the pictures after it dropped", async () => {
  const p = hevcPainter({ refuses: true });
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  await p.draw(batchFrame([{ w: 640, h: 480, payload: [1], keyframe: false }]));
  const said = videoErrors.filter((error) => error !== null);
  assert.equal(said.length, 1);
  assert.match(said[0] ?? "", /the GPU refused the picture/);
  assert.deepEqual(shown, [], "nothing was drawn to show");
  assert.deepEqual([hevc.made, hevc.closed], [1, 1]);
  assert.deepEqual(
    decoded.map((frame) => frame.closed),
    [true, true],
    "a picture not presented is closed all the same",
  );
  assert.deepEqual(
    videoKeyframeAsks,
    [],
    "no keyframe would be presented either",
  );
});

test("the attachment's end gives the software decoder's picture back", async () => {
  const p = hevcPainter();
  await p.draw(batchFrame([{ w: 640, h: 480, payload: KEYFRAME }]));
  p.clear();
  assert.deepEqual(shown, [true, false]);
  assert.deepEqual([hevc.made, hevc.closed], [1, 1]);
});
