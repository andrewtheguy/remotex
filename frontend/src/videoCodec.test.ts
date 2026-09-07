// The one decoder question the client asks, and what each answer selects. The
// property under test is that only a definite "no" reaches for the fallback: a
// browser's "yes", and a browser that cannot answer, both ask for VP9.
import assert from "node:assert/strict";
import { test } from "node:test";

const globals = globalThis as unknown as {
  VideoDecoder: {
    isConfigSupported: (config: {
      codec: string;
    }) => Promise<{ supported?: boolean }>;
  };
};

const { chooseVideoCodec, resetVideoCodecForTests, videoCodec } = await import(
  "./videoCodec.ts"
);

function browserAnswering(
  reply: (codec: string) => Promise<{ supported?: boolean }>,
): void {
  resetVideoCodecForTests();
  globals.VideoDecoder = { isConfigSupported: ({ codec }) => reply(codec) };
}

test("a browser that decodes VP9 asks for VP9", async () => {
  const asked: string[] = [];
  browserAnswering(async (codec) => {
    asked.push(codec);
    return { supported: true };
  });
  assert.equal(await chooseVideoCodec(), "vp9");
  assert.equal(videoCodec(), "vp9");
  // Profile 0, 4:2:0, with every colour field spelled out — the shape the gateway
  // announces for a default target, so the question is about the stream that will
  // actually arrive.
  assert.deepEqual(asked, ["vp09.00.40.08.01.06.06.06.00"]);
});

test("a browser without VP9 asks for H.264", async () => {
  browserAnswering(async () => ({ supported: false }));
  assert.equal(await chooseVideoCodec(), "h264");
  assert.equal(videoCodec(), "h264");
});

test("a browser that cannot answer asks for VP9, since a refusal is the decoder's to make", async () => {
  browserAnswering(async () => {
    throw new TypeError("isConfigSupported disliked the string");
  });
  assert.equal(await chooseVideoCodec(), "vp9");
  // And an answer with no verdict at all is not a "no".
  browserAnswering(async () => ({}));
  assert.equal(await chooseVideoCodec(), "vp9");
});

test("the question is asked once, and the answer is not available before it", async () => {
  let asked = 0;
  browserAnswering(async () => {
    asked += 1;
    return { supported: false };
  });
  assert.throws(() => videoCodec(), /before chooseVideoCodec/);
  assert.equal(await chooseVideoCodec(), "h264");
  assert.equal(await chooseVideoCodec(), "h264");
  assert.equal(asked, 1);
});
