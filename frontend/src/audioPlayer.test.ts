// The player as it drives WebCodecs' decoder: what it configures it with, and
// what a gap in the packets does to it.
//
// No audio is played here. The context is the two calls the player makes of one
// before any sound arrives.
import assert from "node:assert/strict";
import { test } from "node:test";
import { type AudioFormat, createAudioPlayer } from "./audioPlayer.ts";

const context = () =>
  ({ currentTime: 0, close: async () => {} }) as unknown as AudioContext;

/** WebCodecs' decoder as far as the player drives it, and what it was asked. */
class FakeAudioDecoder {
  static calls: string[] = [];
  static configs: AudioDecoderConfig[] = [];
  state = "unconfigured";
  configure(config: AudioDecoderConfig): void {
    this.state = "configured";
    FakeAudioDecoder.calls.push("configure");
    FakeAudioDecoder.configs.push(config);
  }
  decode(chunk: { data: Uint8Array }): void {
    FakeAudioDecoder.calls.push(`decode ${chunk.data[0]}`);
  }
  reset(): void {
    this.state = "unconfigured";
    FakeAudioDecoder.calls.push("reset");
  }
  close(): void {
    this.state = "closed";
  }
}

const OPUS: AudioFormat = {
  codec: "opus",
  sampleRate: 48_000,
  channels: 2,
  packetFrames: 960,
  head: new Uint8Array(19),
};

/** Run `body` with WebCodecs' two globals standing in. */
function withFakeWebCodecs(body: () => void): void {
  const scope = globalThis as Record<string, unknown>;
  const had = [scope.AudioDecoder, scope.EncodedAudioChunk];
  FakeAudioDecoder.calls = [];
  FakeAudioDecoder.configs = [];
  scope.AudioDecoder = FakeAudioDecoder;
  scope.EncodedAudioChunk = class {
    data: Uint8Array;
    constructor(init: { data: Uint8Array }) {
      this.data = init.data;
    }
  };
  try {
    body();
  } finally {
    [scope.AudioDecoder, scope.EncodedAudioChunk] = had;
  }
}

test("a gap starts the decoder again before the packets after it", () => {
  withFakeWebCodecs(() => {
    const player = createAudioPlayer(OPUS, context(), { onError: () => {} });
    player.push([new Uint8Array([1])]);
    player.gap();
    player.push([new Uint8Array([2])]);
    assert.deepEqual(FakeAudioDecoder.calls, [
      "configure",
      "decode 1",
      "reset",
      "configure",
      "decode 2",
    ]);
    player.close();
    player.gap();
    assert.equal(FakeAudioDecoder.calls.length, 5, "nothing after a close");
  });
});

const FLAC: AudioFormat = {
  codec: "flac",
  sampleRate: 44_100,
  channels: 2,
  packetFrames: 882,
  head: new Uint8Array(0),
};

/**
 * A STREAMINFO read the way the FLAC format lays it out, bit by bit, rather
 * than the way the page writes it.
 */
function streamInfo(description: Uint8Array) {
  const bits = (from: number, count: number) => {
    let value = 0;
    for (let at = from; at < from + count; at++) {
      const bit = (description[at >> 3] >> (7 - (at & 7))) & 1;
      value = value * 2 + bit;
    }
    return value;
  };
  return {
    marker: new TextDecoder().decode(description.subarray(0, 4)),
    last: bits(32, 1),
    type: bits(33, 7),
    length: bits(40, 24),
    minBlock: bits(64, 16),
    maxBlock: bits(80, 16),
    rate: bits(144, 20),
    channels: bits(164, 3) + 1,
    sampleBits: bits(167, 5) + 1,
    total: bits(172, 36),
    size: description.length,
  };
}

test("a FLAC stream is decoded by WebCodecs, under the header the page writes", () => {
  withFakeWebCodecs(() => {
    const player = createAudioPlayer(FLAC, context(), { onError: () => {} });
    player.push([new Uint8Array([1])]);
    player.gap();
    player.push([new Uint8Array([2])]);
    // A frame decodes alone, and a gap starts the decoder again all the same.
    assert.deepEqual(FakeAudioDecoder.calls, [
      "configure",
      "decode 1",
      "reset",
      "configure",
      "decode 2",
    ]);
    const config = FakeAudioDecoder.configs[0];
    assert.equal(config.codec, "flac");
    assert.equal(config.sampleRate, 44_100);
    assert.equal(config.numberOfChannels, 2);
    assert.deepEqual(streamInfo(config.description as Uint8Array), {
      marker: "fLaC",
      last: 1,
      type: 0,
      length: 34,
      minBlock: 882,
      maxBlock: 882,
      rate: 44_100,
      channels: 2,
      sampleBits: 16,
      total: 0,
      size: 42,
    });
    player.close();
  });
});
