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
  resets = 0;
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

  // "Resets all states including configuration", which the real one means literally:
  // a decoder that has been reset and not configured again decodes nothing, silently.
  reset() {
    this.resets += 1;
    this.state = "unconfigured";
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
  const table = createVideoStreams(
    {
      onError: (reason) => errors.push(reason),
      onStalled: (reason) => stalls.push(reason),
    },
    STALL_MS,
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
    s.decoder().resets,
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

test("the reset stream waits for its keyframe rather than erroring per frame", async () => {
  const s = streams();
  await s.table.decode(1, size, unit(1), true);
  const decoder = s.decoder();
  assert.equal(decoder.chunks.length, 1);
  // The reset took the configuration with it, so the stall has to hand it back —
  // an unconfigured decoder refuses the keyframe below and every unit after it.
  assert.equal(decoder.configures, 2, "the reset stream was left unconfigured");
  assert.equal(decoder.state, "configured");

  // The frames still arriving for a region whose chain was just cut. They are
  // expressed against pictures the decoder no longer has.
  assert.equal(await s.table.decode(1, size, unit(2), false), null);
  assert.equal(await s.table.decode(1, size, unit(3), false), null);
  assert.equal(
    decoder.chunks.length,
    1,
    "deltas were handed to a decoder with no history",
  );

  // The repaint the stall asked for. The same decoder takes it — a stall keeps the
  // configuration, so the stream picks up where the keyframe puts it.
  const frame = s.table.decode(1, size, unit(4), true);
  assert.equal(decoder.chunks.length, 2);
  assert.equal(decoder.chunks[1].type, "key");
  decoder.emit(0xb2);
  assert.equal(tagOf(await frame), 0xb2);
  assert.equal(s.stalls.length, 1, "one stall, asked about once");
});

test("only the silent stream is reset; a sibling keeps decoding", async () => {
  const s = streams();
  s.table.setFormat(2, { decode: "vp09.00.40.08" });
  const quiet = s.table.decode(1, size, unit(1), true);
  const one = s.decoder();
  const busy = s.table.decode(2, size, unit(2), true);
  const two = s.decoder();
  two.emit(0xc3);

  assert.equal(tagOf(await busy), 0xc3);
  assert.equal(await quiet, null);
  assert.equal(one.resets, 1);
  assert.equal(
    two.resets,
    0,
    "a region that answered was reset with the one that did not",
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
