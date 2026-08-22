// The decoder table's liveness, driven against a fake `VideoDecoder`.
//
// What is pinned here is the one property the rest of the paint path assumes and
// WebCodecs does not promise: that `decode()` settles. A decoder that produces no
// output for a chunk — and raises no error about it — is a permanent session freeze
// if the promise it handed out is the paint worker's next `await`, because the worker
// draws one batch at a time and a batch carries at most one unit per stream, so no
// later frame ever comes along to shake the queue loose.
//
// Run with `bun test src/videoDecoder.test.ts` from frontend/.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { createVideoStreams } from "./videoDecoder.ts";

/** A frame the fake decoder emitted, so a test can see it was handed over and closed. */
interface FakeFrame {
  tag: number;
  closed: boolean;
}

/** Every decoder built, newest last, so a test can drive one directly. */
let built: FakeDecoder[] = [];

// Silent by default: emitting nothing for a chunk is the failure being reproduced,
// so it is this fake's normal behaviour and `emit` is the explicit opposite.
class FakeDecoder {
  readonly output: (frame: unknown) => void;
  readonly chunks: { type: string }[] = [];
  state = "unconfigured";
  configures = 0;
  closes = 0;

  constructor(init: {
    output: (frame: unknown) => void;
    error: (error: Error) => void;
  }) {
    this.output = init.output;
    built.push(this);
  }

  configure() {
    this.configures += 1;
    this.state = "configured";
  }

  decode(chunk: { type: string }) {
    this.chunks.push(chunk);
  }

  close() {
    this.closes += 1;
    this.state = "closed";
  }

  /** One decoded picture, as the browser would deliver it. */
  emit(tag: number): FakeFrame {
    const frame: FakeFrame = { tag, closed: false };
    this.output({
      close() {
        frame.closed = true;
      },
      get tag() {
        return frame.tag;
      },
    });
    return frame;
  }
}

const globals = globalThis as unknown as {
  VideoDecoder: unknown;
  EncodedVideoChunk: unknown;
};

beforeEach(() => {
  built = [];
  globals.VideoDecoder = FakeDecoder;
  globals.EncodedVideoChunk = class {
    type: string;
    constructor(init: { type: string }) {
      this.type = init.type;
    }
  };
});

afterEach(() => {
  globals.VideoDecoder = undefined;
  globals.EncodedVideoChunk = undefined;
});

/** Which picture came back, through the `VideoFrame` shape the caller is typed to. */
const tagOf = (frame: VideoFrame | null) =>
  (frame as unknown as FakeFrame | null)?.tag;

const unit = (byte: number) => new Uint8Array([byte]);
const size = { w: 320, h: 240 };
// Short enough to wait out, and the reason the deadline is a parameter: the real
// one is a liveness backstop measured in seconds, not something to sleep through.
const STALL_MS = 10;
const afterStall = () => new Promise((resolve) => setTimeout(resolve, 30));
// The same bargain for the retirement clock: the real one is four seconds, and what
// is under test is that it is a clock at all rather than how long it is.
const RETIRE_MS = 10;
const afterRetire = () => new Promise((resolve) => setTimeout(resolve, 30));

function streams() {
  const errors: string[] = [];
  const stalls: string[] = [];
  const table = createVideoStreams(
    {
      onError: (reason) => errors.push(reason),
      onNeedsKeyframe: (reason) => stalls.push(reason),
    },
    STALL_MS,
    RETIRE_MS,
  );
  table.setFormat(1, { decode: "vp09.00.40.08" });
  return { table, errors, stalls, decoder: () => built[built.length - 1] };
}

test("a decode settles when its frame arrives", async () => {
  const s = streams();
  const frame = s.table.decode(1, size, unit(1), true);
  const emitted = s.decoder().emit(0xa1);
  assert.equal(tagOf(await frame), 0xa1);
  assert.equal(
    emitted.closed,
    false,
    "the caller owns the frame and closes it",
  );
  assert.deepEqual(s.stalls, [], "a decoder that answered has not stalled");
});

test("a decoder that answers nothing settles anyway, and asks for a keyframe", async () => {
  const s = streams();
  const frame = s.table.decode(1, size, unit(1), true);
  assert.equal(
    await frame,
    null,
    "the paint chain must not be left holding this",
  );
  assert.equal(
    s.decoder().closes,
    1,
    "a late frame could still resolve a later unit",
  );
  assert.equal(s.stalls.length, 1);
  assert.match(
    s.stalls[0],
    /^stream 1: /,
    "a stall says which region went quiet",
  );
  assert.deepEqual(s.errors, [], "nothing here is a decoder error");
});

test("the stalled stream waits for its keyframe rather than erroring per frame", async () => {
  const s = streams();
  await s.table.decode(1, size, unit(1), true);
  const wedged = s.decoder();
  assert.equal(wedged.chunks.length, 1);
  // The stall threw the decoder away: reconfiguring the one that went quiet flushes
  // its wedged pipeline, and Chromium answers that flush with a second failure.
  assert.equal(wedged.closes, 1, "the quiet decoder was kept");

  // The frames still arriving for a region whose chain was just cut. They are
  // expressed against pictures no decoder here has, so the fresh decoder they build
  // is handed nothing until the keyframe.
  assert.equal(await s.table.decode(1, size, unit(2), false), null);
  assert.equal(await s.table.decode(1, size, unit(3), false), null);
  const fresh = s.decoder();
  assert.notEqual(fresh, wedged, "the stalled decoder was reused");
  assert.equal(
    fresh.chunks.length,
    0,
    "deltas were handed to a decoder with no history",
  );

  // The repaint the stall asked for, and where the fresh decoder starts.
  const frame = s.table.decode(1, size, unit(4), true);
  assert.equal(fresh.chunks.length, 1);
  assert.equal(fresh.chunks[0].type, "key");
  fresh.emit(0xb2);
  assert.equal(tagOf(await frame), 0xb2);
  assert.equal(s.stalls.length, 1, "one stall, asked about once");
});

test("only the silent stream is discarded; a sibling keeps decoding", async () => {
  const s = streams();
  s.table.setFormat(2, { decode: "vp09.00.40.08" });
  const quiet = s.table.decode(1, size, unit(1), true);
  const one = s.decoder();
  const busy = s.table.decode(2, size, unit(2), true);
  const two = s.decoder();
  two.emit(0xc3);

  assert.equal(tagOf(await busy), 0xc3);
  assert.equal(await quiet, null);
  assert.equal(one.closes, 1);
  assert.equal(
    two.closes,
    0,
    "a region that answered was discarded with the one that did not",
  );
  assert.equal(s.stalls.length, 1);
});

test("closing the table settles what the decoders owe", async () => {
  const s = streams();
  const frame = s.table.decode(1, size, unit(1), true);
  s.table.close();
  assert.equal(await frame, null);
  assert.equal(s.decoder().closes, 1);
  // The wedged draw is already free; the backstop must not then fire on a table
  // that no longer has anything to say.
  await afterStall();
  assert.deepEqual(s.stalls, []);
});

test("an ended stream keeps its decoder until the retirement clock runs out", async () => {
  const s = streams();
  const frame = s.table.decode(1, size, unit(1), true);
  const decoder = s.decoder();
  decoder.emit(0xa1);
  await frame;

  s.table.end(1);
  assert.equal(
    decoder.closes,
    0,
    "closing on the spot rebuilds a decoder the next region could have reused",
  );

  // The region came back before the clock ran out — the common case on this dial —
  // and it decodes on the decoder that was already there.
  const again = s.table.decode(1, size, unit(2), true);
  assert.equal(
    s.decoder(),
    decoder,
    "a returning region paid for a new decoder",
  );
  decoder.emit(0xb2);
  assert.equal(tagOf(await again), 0xb2);

  // And having come back, it is not retired out from under itself later.
  await afterRetire();
  assert.equal(decoder.closes, 0, "the cancelled retirement still fired");
});

test("a stream that does not come back gives its decode session up", async () => {
  const s = streams();
  const frame = s.table.decode(1, size, unit(1), true);
  const decoder = s.decoder();
  decoder.emit(0xa1);
  await frame;

  s.table.end(1);
  await afterRetire();
  assert.equal(
    decoder.closes,
    1,
    "the decode session was held for the session",
  );

  // The format went with it: the next stream on this id announces its own before its
  // first unit, and a stale one would configure the wrong picture.
  assert.equal(await s.table.decode(1, size, unit(2), true), null);
  assert.equal(
    s.decoder(),
    decoder,
    "a unit with no format must not build a decoder",
  );

  s.table.setFormat(1, { decode: "vp09.00.40.08" });
  const again = s.table.decode(1, size, unit(3), true);
  assert.notEqual(
    s.decoder(),
    decoder,
    "the new region got a decoder of its own",
  );
  s.decoder().emit(0xc3);
  assert.equal(tagOf(await again), 0xc3);
});

test("a format announced during retirement outlives the clock", async () => {
  const s = streams();
  const frame = s.table.decode(1, size, unit(1), true);
  const decoder = s.decoder();
  decoder.emit(0xa1);
  await frame;

  // The gateway ended this id and handed it straight back: the new stream's format
  // arrives first, and its first unit may be any distance behind it.
  s.table.end(1);
  s.table.setFormat(1, { decode: "vp09.00.40.08" });
  await afterRetire();
  assert.equal(decoder.closes, 0, "the announced stream lost its decoder");

  const again = s.table.decode(1, size, unit(2), true);
  assert.equal(
    s.decoder(),
    decoder,
    "a same-size stream paid for a new decoder",
  );
  decoder.emit(0xb2);
  assert.equal(
    tagOf(await again),
    0xb2,
    "the unit was dropped for want of its format",
  );
});

test("ending one stream leaves the others decoding", async () => {
  const s = streams();
  s.table.setFormat(2, { decode: "vp09.00.40.08" });
  const settled = s.table.decode(1, size, unit(1), true);
  const one = s.decoder();
  const kept = s.table.decode(2, size, unit(1), true);
  const two = s.decoder();
  // Both answered, so nothing here is owed and the stall backstop stays out of it.
  one.emit(0xa1);
  two.emit(0xc3);
  await settled;
  assert.equal(tagOf(await kept), 0xc3);

  s.table.end(1);
  await afterRetire();
  assert.equal(one.closes, 1);
  assert.equal(
    two.closes,
    0,
    "a sibling's region ending is not this one's business",
  );

  // And stream 2 is still the decoder it was, decoding.
  const more = s.table.decode(2, size, unit(2), false);
  assert.equal(s.decoder(), two);
  two.emit(0xd4);
  assert.equal(tagOf(await more), 0xd4);
});
