// The passthrough conversion, which is the only part of the player besides the
// schedule that a test can reach: everything around it is an AudioContext, a
// WebCodecs decoder and Web Audio nodes, none of which exist here.
//
// It is worth reaching. Under `audio_codec = "pcm"` the gateway does nothing to
// the remote's samples at all, so this is the single place where a sample can be
// got wrong — a byte order, a channel order or a scale factor — and the only
// place a test can catch it before a browser plays it as noise.
//
// Run with `bun test src/audioPlayer.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { needsDecoder, PCM_CODEC, pcmChannels } from "./audioPlayer.ts";

/** Interleaved little-endian i16, the way the gateway sends it. */
function wave(frames: number[][]): Uint8Array {
  const bytes = new Uint8Array(frames.length * frames[0].length * 2);
  const view = new DataView(bytes.buffer);
  let at = 0;
  for (const frame of frames) {
    for (const sample of frame) {
      view.setInt16(at, sample, true);
      at += 2;
    }
  }
  return bytes;
}

test("the two codecs are told apart by whether they need a decoder", () => {
  const base = {
    sampleRate: 48_000,
    channels: 2,
    packetFrames: 960,
    head: new Uint8Array(),
  };
  assert.equal(needsDecoder({ ...base, codec: "opus" }), true);
  assert.equal(needsDecoder({ ...base, codec: PCM_CODEC }), false);
  // Anything unrecognised goes to WebCodecs, which reports what it cannot decode.
  // Guessing the other way would mean playing an encoded stream as raw samples.
  assert.equal(needsDecoder({ ...base, codec: "mp4a.40.2" }), true);
});

test("interleaved samples are split back into their own channels", () => {
  // Hard-panned, which is the only input that tells a real split from a blend:
  // a dual-mono source looks identical either way. See the same reasoning in
  // src/opus_stream.rs's round-trip test.
  const planes = pcmChannels(
    wave([
      [12_000, 0],
      [-12_000, 0],
      [12_000, 0],
    ]),
    2,
  );
  assert.equal(planes.length, 2);
  assert.deepEqual(Array.from(planes[1]), [0, 0, 0], "right is silent");
  assert.ok(
    planes[0][0] > 0.36 && planes[0][0] < 0.37,
    `left carries it: ${planes[0][0]}`,
  );
  assert.ok(planes[0][1] < 0, "and its sign survives");
});

test("full scale comes back as full scale, and the ends do not clip", () => {
  // -32768 has to land on exactly -1.0: dividing by 32767 to make +1.0 reachable
  // would put it past the floor instead, which is a click on every peak.
  const planes = pcmChannels(wave([[-32_768, 32_767]]), 2);
  assert.equal(planes[0][0], -1);
  assert.ok(
    planes[1][0] < 1 && planes[1][0] > 0.9999,
    `just under +1: ${planes[1][0]}`,
  );
});

test("samples are read little-endian, the way the wire carries them", () => {
  // 0x0100 big-endian is 256; little-endian it is 1. A byte order read the wrong
  // way round is still perfectly valid audio, which is what makes it worth pinning.
  const planes = pcmChannels(new Uint8Array([0x01, 0x00, 0x00, 0x00]), 2);
  assert.equal(planes[0][0], 1 / 32_768);
});

test("a packet is read from its own bytes, not its buffer's", () => {
  // Packets arrive as subarrays of one binary frame (see decodeAudioFrame), so a
  // view built from `.buffer` alone reads whatever packet came first.
  const frame = wave([
    [999, 999],
    [-4, 8],
  ]);
  const second = frame.subarray(4);
  assert.equal(second.byteOffset, 4, "a view into the frame, not a copy");
  const planes = pcmChannels(second, 2);
  assert.deepEqual(Array.from(planes[0]), [-4 / 32_768]);
  assert.deepEqual(Array.from(planes[1]), [8 / 32_768]);
});

test("a trailing part-frame is dropped rather than misread", () => {
  // Two whole frames and three bytes of a third. Reading the remainder would
  // shift every channel against the others for the rest of the packet.
  const packet = new Uint8Array(11);
  const planes = pcmChannels(packet, 2);
  assert.equal(planes[0].length, 2);
  assert.equal(planes[1].length, 2);
});

test("mono is not assumed to be stereo", () => {
  const planes = pcmChannels(wave([[1_000], [2_000]]), 1);
  assert.equal(planes.length, 1);
  assert.equal(planes[0].length, 2, "both frames, not one interleaved pair");
});
