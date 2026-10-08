// The one decoder question the client asks, and what each answer selects. The
// property under test is that only a definite "no" gives up the colour: a browser's
// "yes", and a browser that cannot answer, both ask for 4:4:4.
import assert from "node:assert/strict";
import { test } from "node:test";

const globals = globalThis as unknown as {
  VideoDecoder: {
    isConfigSupported: (config: {
      codec: string;
    }) => Promise<{ supported?: boolean }>;
  };
};

const { chooseVideoChroma, resetVideoChromaForTests, videoChroma } =
  await import("./videoChroma.ts");

function browserAnswering(
  reply: (codec: string) => Promise<{ supported?: boolean }>,
): void {
  resetVideoChromaForTests();
  globals.VideoDecoder = { isConfigSupported: ({ codec }) => reply(codec) };
}

test("a browser that decodes profile 1 asks for 4:4:4", async () => {
  const asked: string[] = [];
  browserAnswering(async (codec) => {
    asked.push(codec);
    return { supported: true };
  });
  assert.equal(await chooseVideoChroma(), "444");
  assert.equal(videoChroma(), "444");
  // Profile 1, 4:4:4, with every colour field spelled out — the shape the gateway
  // announces for a 4:4:4 stream, so the question is about the stream that will
  // actually arrive.
  assert.deepEqual(asked, ["vp09.01.40.08.03.06.06.06.00"]);
});

test("a browser without profile 1 asks for 4:2:0", async () => {
  browserAnswering(async () => ({ supported: false }));
  assert.equal(await chooseVideoChroma(), "420");
  assert.equal(videoChroma(), "420");
});

test("a browser that cannot answer asks for 4:4:4, since a refusal is the decoder's to make", async () => {
  browserAnswering(async () => {
    throw new TypeError("isConfigSupported disliked the string");
  });
  assert.equal(await chooseVideoChroma(), "444");
  // And an answer with no verdict at all is not a "no".
  browserAnswering(async () => ({}));
  assert.equal(await chooseVideoChroma(), "444");
});

test("the question is asked once, and the answer is not available before it", async () => {
  let asked = 0;
  browserAnswering(async () => {
    asked += 1;
    return { supported: false };
  });
  assert.throws(() => videoChroma(), /before chooseVideoChroma/);
  assert.equal(await chooseVideoChroma(), "420");
  assert.equal(await chooseVideoChroma(), "420");
  assert.equal(asked, 1);
});
