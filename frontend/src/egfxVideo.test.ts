// A passed pipeline's H.264 decoders, driven against a fake `VideoDecoder`: one for
// each surface, each unit's picture awaited, and every wait settled on every path.
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import type { H264Unit } from "./egfxCompositor.ts";
import { createEgfxVideo, drawnAsCoded, rdpH264Config } from "./egfxVideo.ts";

/** The colour a host's H.264 is: what each decoder is told it is. */
const RDP_COLOUR = {
  primaries: "bt709",
  transfer: "iec61966-2-1",
  matrix: "bt709",
  fullRange: true,
} as const;

interface Chunk {
  type: string;
  timestamp: number;
  data: Uint8Array;
}

class FakeDecoder {
  state = "unconfigured";
  configured: VideoDecoderConfig[] = [];
  chunks: Chunk[] = [];
  /** What `decode` does with a chunk: by default, nothing until told. */
  onDecode: (chunk: Chunk) => void = () => {};
  init: {
    output: (frame: VideoFrame) => void;
    error: (error: DOMException) => void;
  };
  constructor(init: FakeDecoder["init"]) {
    this.init = init;
    decoders.push(this);
  }
  configure(config: VideoDecoderConfig) {
    this.configured.push(config);
    this.state = "configured";
  }
  decode(chunk: Chunk) {
    this.chunks.push(chunk);
    this.onDecode(chunk);
  }
  close() {
    this.state = "closed";
  }
  /** Hand back a picture for the chunk last decoded. */
  output(): { closed: boolean; of: number } {
    const frame = {
      closed: false,
      of: this.chunks.at(-1)?.data[0] ?? -1,
      close() {
        this.closed = true;
      },
    };
    this.init.output(frame as unknown as VideoFrame);
    return frame;
  }
}

let decoders: FakeDecoder[] = [];

const page = globalThis as unknown as {
  VideoDecoder?: unknown;
  EncodedVideoChunk?: unknown;
};
const original = {
  VideoDecoder: page.VideoDecoder,
  EncodedVideoChunk: page.EncodedVideoChunk,
};

beforeEach(() => {
  decoders = [];
  page.VideoDecoder = Object.assign(FakeDecoder, {
    isConfigSupported: (config: VideoDecoderConfig) =>
      Promise.resolve({
        supported: !config.codec.endsWith("ff"),
      }),
  });
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
});

afterEach(() => {
  page.VideoDecoder = original.VideoDecoder;
  page.EncodedVideoChunk = original.EncodedVideoChunk;
});

const key = (surface: number, codec = "avc1.4d4020"): H264Unit => ({
  surface,
  start: 0,
  end: 1,
  key: true,
  codec,
  window: "whole",
});
const delta = (surface: number): H264Unit => ({
  ...key(surface),
  key: false,
  codec: null,
});

/** Let the decode's own awaits run, up to the unit being handed to the decoder. */
const submitted = async (decoder: () => FakeDecoder | undefined, n: number) => {
  while ((decoder()?.chunks.length ?? 0) < n) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
};

test("each surface's stream goes through a decoder of its own, a picture a unit", async () => {
  const video = createEgfxVideo();
  const first = video.decode(key(1), Uint8Array.of(11));
  await submitted(() => decoders[0], 1);
  assert.deepEqual(decoders[0].configured, [
    { codec: "avc1.4d4020", optimizeForLatency: true, colorSpace: RDP_COLOUR },
  ]);
  const picture = decoders[0].output();
  assert.equal(await first, picture as unknown as VideoFrame);

  // The next unit of the stream: the same decoder, not configured again.
  const second = video.decode(delta(1), Uint8Array.of(12));
  await submitted(() => decoders[0], 2);
  decoders[0].output();
  await second;
  assert.equal(decoders.length, 1);
  assert.equal(decoders[0].configured.length, 1);
  assert.deepEqual(
    decoders[0].chunks.map((chunk) => [chunk.type, chunk.timestamp]),
    [
      ["key", 0],
      ["delta", 1],
    ],
  );

  // Another surface is another stream.
  const other = video.decode(key(2), Uint8Array.of(21));
  await submitted(() => decoders[1], 1);
  decoders[1].output();
  await other;
  assert.equal(decoders.length, 2);
  video.close();
  assert.deepEqual(
    decoders.map((decoder) => decoder.state),
    ["closed", "closed"],
  );
});

test("a decoder is configured for the stream's own profile, the browser's choice of decoder", async () => {
  assert.deepEqual(await rdpH264Config("avc1.640028"), {
    codec: "avc1.640028",
    optimizeForLatency: true,
    colorSpace: RDP_COLOUR,
  });
  assert.equal(await rdpH264Config("avc1.4d40ff"), null);
});

test("a picture is drawn as coded only labelled with the stream's colour, at the size coded", () => {
  const picture = (colorSpace: VideoColorSpaceInit, displayWidth = 64) => ({
    colorSpace: colorSpace as VideoColorSpace,
    visibleRect: { x: 0, y: 0, width: 64, height: 48 } as DOMRectReadOnly,
    displayWidth,
    displayHeight: 48,
  });
  assert.equal(drawnAsCoded(picture(RDP_COLOUR)), true);
  // As Chrome's hardware decoder on a Mac labels it, and its software decoder.
  assert.equal(
    drawnAsCoded(
      picture({ ...RDP_COLOUR, transfer: "bt709", fullRange: false }),
    ),
    false,
  );
  assert.equal(
    drawnAsCoded(picture({ ...RDP_COLOUR, matrix: "smpte170m" })),
    false,
  );
  assert.equal(drawnAsCoded(picture(RDP_COLOUR, 128)), false, "scaled");
});

test("a keyframe that names another profile configures the decoder again", async () => {
  const video = createEgfxVideo();
  const first = video.decode(key(1), Uint8Array.of(1));
  await submitted(() => decoders[0], 1);
  decoders[0].output();
  await first;
  const again = video.decode(key(1, "avc1.640028"), Uint8Array.of(2));
  await submitted(() => decoders[0], 2);
  decoders[0].output();
  await again;
  assert.deepEqual(
    decoders[0].configured.map((config) => config.codec),
    ["avc1.4d4020", "avc1.640028"],
  );
  video.close();
});

test("a stream that does not start at a keyframe, or that no decoder takes, is refused", async () => {
  const video = createEgfxVideo();
  await assert.rejects(
    video.decode(delta(1), Uint8Array.of(1)),
    /did not start at a keyframe/,
  );
  await assert.rejects(
    video.decode(key(2, "avc1.4d40ff"), Uint8Array.of(1)),
    /no decoder for avc1\.4d40ff/,
  );
  video.close();
});

test("a decoder that fails, or is closed, settles the unit waiting on it", async () => {
  const video = createEgfxVideo();
  const failing = video.decode(key(1), Uint8Array.of(1));
  await submitted(() => decoders[0], 1);
  decoders[0].init.error(new DOMException("bad data", "EncodingError"));
  await assert.rejects(failing, /H\.264 decoder failed: bad data/);

  const dropped = video.decode(key(2), Uint8Array.of(1));
  await submitted(() => decoders[1], 1);
  video.drop(2);
  await assert.rejects(dropped, /its surface was deleted/);
  assert.equal(decoders[1].state, "closed");

  const closing = video.decode(key(3), Uint8Array.of(1));
  await submitted(() => decoders[2], 1);
  video.close();
  await assert.rejects(closing, /decoders are closed/);
  await assert.rejects(
    video.decode(key(4), Uint8Array.of(1)),
    /decoders are closed/,
  );
});

test("a decoder that gives no picture in the time it is given is given up on", async () => {
  const video = createEgfxVideo(10);
  await assert.rejects(
    video.decode(key(1), Uint8Array.of(1)),
    /gave no picture for a unit/,
  );
  // A picture that comes after that is nothing's, and is closed.
  assert.equal(decoders[0].output().closed, true);
  video.close();
});

test("a surface's stream that ended starts over on a new decoder", async () => {
  const video = createEgfxVideo();
  const first = video.decode(key(1), Uint8Array.of(1));
  await submitted(() => decoders[0], 1);
  decoders[0].output();
  await first;
  video.drop(1);
  assert.equal(decoders[0].state, "closed");
  await assert.rejects(
    video.decode(delta(1), Uint8Array.of(2)),
    /did not start at a keyframe/,
    "the new stream has had no keyframe",
  );
  video.close();
});

test("a picture nothing waits for is closed", async () => {
  const video = createEgfxVideo();
  const first = video.decode(key(1), Uint8Array.of(1));
  await submitted(() => decoders[0], 1);
  decoders[0].output();
  await first;
  const extra = decoders[0].output();
  assert.equal(extra.closed, true);
  video.close();
});
