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
import {
  resetSoftwareDecodersForTests,
  type VideoDecoderLike,
  type VideoDecoderLikeInit,
} from "./softwareDecoder.ts";
import { createDesktopVideo, createVideoStream } from "./videoDecoder.ts";

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

/** Which picture came back, through whatever shape the caller is typed to. */
const tagOf = (frame: unknown) => (frame as FakeFrame | null)?.tag;

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

test("a unit that completes no picture settles to null, and the next picture is its own unit's", async () => {
  // A software decoder answers every unit, a none included.
  let init: VideoDecoderLikeInit | undefined;
  const stalls: string[] = [];
  const stream = createVideoStream(
    { decode: "hev1.4.10.L150.BE.8" },
    {
      onError: () => assert.fail("no error"),
      onNeedsKeyframe: (r) => stalls.push(r),
    },
    STALL_MS,
    (given) => {
      init = given;
      return new FakeDecoder(given as never) as unknown as VideoDecoderLike;
    },
  );
  const first = stream.decode(unit(1), 0, true);
  const second = stream.decode(unit(2), 1, false);
  init?.noPicture?.();
  assert.equal(await first, null);
  built[0].emit(0xc3);
  assert.equal(
    tagOf(await second),
    0xc3,
    "the picture landed on the wrong unit",
  );
  await afterStall();
  assert.deepEqual(stalls, [], "a none is an answer, not a stall");
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

test("a stream is decoded by the page where the gateway says so, by the module that decodes it", async () => {
  // The decode workers made, by name: a software decoder starts its module's.
  const workers: string[] = [];
  const globals = globalThis as unknown as { Worker: unknown };
  const before = globals.Worker;
  globals.Worker = class {
    onmessage = null;
    onerror = null;
    constructor(_url: URL, options: { name: string }) {
      workers.push(options.name);
    }
    postMessage() {}
    terminate() {}
  };
  const VP9_444 = "vp09.01.40.08.03.06.06.06.00";
  const VP9_420 = "vp09.00.40.08.01.06.06.06.00";
  const HEVC = "hev1.4.10.L150.BE.8";
  const BOTH = { hevc: true, vp9: true };
  try {
    // The format, what the page runs, then the worker started, or "browser" for
    // its own decoder, or null for a stream said and not decoded.
    const cases: [
      { decode: string; software?: boolean },
      { hevc: boolean; vp9: boolean },
      string | null,
    ][] = [
      [{ decode: VP9_444, software: true }, BOTH, "vp9-decoder"],
      [{ decode: HEVC, software: true }, BOTH, "hevc-decoder"],
      // Not said: the browser's own, whatever the page could run.
      [{ decode: VP9_444, software: false }, BOTH, "browser"],
      [{ decode: VP9_444 }, BOTH, "browser"],
      [{ decode: HEVC, software: false }, BOTH, "browser"],
      // Said of a stream the page cannot run the decoder of: no other stands in.
      [{ decode: VP9_444, software: true }, { hevc: true, vp9: false }, null],
      [{ decode: HEVC, software: true }, { hevc: false, vp9: true }, null],
      // Said of a stream no module decodes.
      [{ decode: VP9_420, software: true }, BOTH, null],
    ];
    for (const [format, runs, expected] of cases) {
      resetSoftwareDecodersForTests();
      const browsers = built.length;
      workers.length = 0;
      const errors: [string, boolean][] = [];
      const table = createDesktopVideo(
        {
          onError: (reason, recoverable) => errors.push([reason, recoverable]),
          onNeedsKeyframe: () => {},
        },
        STALL_MS,
        runs,
      );
      table.setFormat(format);
      void table.decode(size, unit(1), true);
      void table.decode(size, unit(2), false);
      const which = JSON.stringify([format, runs]);
      const software = expected !== null && expected !== "browser";
      assert.deepEqual(workers, software ? [expected] : [], which);
      assert.equal(
        built.length - browsers,
        expected === "browser" ? 1 : 0,
        which,
      );
      // Said once for the format, and as a refusal no keyframe repairs.
      // (This file's chunks are not ones a decode worker could be posted, so what
      // a software decoder then does with one is softwareDecoder.test.ts's.)
      if (!software) {
        assert.equal(errors.length, expected === null ? 1 : 0, which);
      }
      if (expected === null) {
        assert.equal(errors[0][1], false, which);
        assert.match(errors[0][0], /decoded by this page/, which);
      }
      table.close();
      resetSoftwareDecodersForTests();
    }
  } finally {
    globals.Worker = before;
  }
});

test("a format that says otherwise of the same stream replaces the decoder", async () => {
  const table = createDesktopVideo(
    { onError: () => {}, onNeedsKeyframe: () => {} },
    STALL_MS,
  );
  const decode = "vp09.01.40.08.03.06.06.06.00";
  table.setFormat({ decode, software: false });
  void table.decode(size, unit(1), true);
  const first = built[built.length - 1];
  table.setFormat({ decode, software: false });
  assert.equal(first.closes, 0, "the same format cost a decoder");
  table.setFormat({ decode, software: true });
  assert.equal(first.closes, 1);
  table.close();
});
