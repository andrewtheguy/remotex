// The desktop decoder's liveness, driven against a fake `VideoDecoder`.
//
// What is pinned here is the one property the rest of the paint path assumes and
// WebCodecs does not promise: that `decode()` settles. A decoder that produces no
// output for a chunk — and raises no error about it — is a permanent session freeze
// if the promise it handed out is the paint worker's next `await`, because the worker
// draws one batch at a time, so no later frame ever comes along to shake the queue
// loose.
//
// Run with `bun test src/videoDecoder.test.ts` from frontend/.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { createDesktopVideo } from "./videoDecoder.ts";

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

function streams() {
  const errors: string[] = [];
  const stalls: string[] = [];
  const table = createDesktopVideo(
    {
      onError: (reason) => errors.push(reason),
      onNeedsKeyframe: (reason) => stalls.push(reason),
    },
    STALL_MS,
  );
  table.setFormat({ decode: "vp09.00.40.08" });
  return { table, errors, stalls, decoder: () => built[built.length - 1] };
}

test("a decode settles when its frame arrives", async () => {
  const s = streams();
  const frame = s.table.decode(size, unit(1), true);
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
  const frame = s.table.decode(size, unit(1), true);
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
  assert.deepEqual(s.errors, [], "nothing here is a decoder error");
});

test("the stalled stream waits for its keyframe rather than erroring per frame", async () => {
  const s = streams();
  await s.table.decode(size, unit(1), true);
  const wedged = s.decoder();
  assert.equal(wedged.chunks.length, 1);
  // The stall threw the decoder away: reconfiguring the one that went quiet flushes
  // its wedged pipeline, and Chromium answers that flush with a second failure.
  assert.equal(wedged.closes, 1, "the quiet decoder was kept");

  // The frames still arriving for a stream whose chain was just cut. They are
  // expressed against pictures no decoder here has, so the fresh decoder they build
  // is handed nothing until the keyframe.
  assert.equal(await s.table.decode(size, unit(2), false), null);
  assert.equal(await s.table.decode(size, unit(3), false), null);
  const fresh = s.decoder();
  assert.notEqual(fresh, wedged, "the stalled decoder was reused");
  assert.equal(
    fresh.chunks.length,
    0,
    "deltas were handed to a decoder with no history",
  );

  // The repaint the stall asked for, and where the fresh decoder starts.
  const frame = s.table.decode(size, unit(4), true);
  assert.equal(fresh.chunks.length, 1);
  assert.equal(fresh.chunks[0].type, "key");
  fresh.emit(0xb2);
  assert.equal(tagOf(await frame), 0xb2);
  assert.equal(s.stalls.length, 1, "one stall, asked about once");
});

test("closing settles what the decoder owes", async () => {
  const s = streams();
  const frame = s.table.decode(size, unit(1), true);
  s.table.close();
  assert.equal(await frame, null);
  assert.equal(s.decoder().closes, 1);
  // The wedged draw is already free; the backstop must not then fire on a table
  // that no longer has anything to say.
  await afterStall();
  assert.deepEqual(s.stalls, []);
});

test("a unit before its format is dropped without building a decoder", async () => {
  const table = createDesktopVideo(
    { onError: () => {}, onNeedsKeyframe: () => {} },
    STALL_MS,
  );
  const before = built.length;
  assert.equal(await table.decode(size, unit(1), true), null);
  assert.equal(built.length, before, "a unit with no format built a decoder");
});

test("a new picture size replaces the decoder", async () => {
  const s = streams();
  const first = s.table.decode(size, unit(1), true);
  const decoder = s.decoder();
  decoder.emit(0xa1);
  await first;

  const resized = s.table.decode({ w: 640, h: 480 }, unit(2), true);
  assert.notEqual(s.decoder(), decoder, "a resized picture reused the decoder");
  assert.equal(decoder.closes, 1);
  s.decoder().emit(0xb2);
  assert.equal(tagOf(await resized), 0xb2);
});
