// The one question the page asks about the browser's own VP9 decoder, and what each
// answer means. The property under test is that only a definite "no" hands the
// stream to the page's WebAssembly decoder: a browser's "yes", and a browser that
// cannot answer, both keep its own decoder.
import assert from "node:assert/strict";
import { test } from "node:test";

const globals = globalThis as unknown as {
  VideoDecoder: {
    isConfigSupported: (config: {
      codec: string;
    }) => Promise<{ supported?: boolean }>;
  };
};

const { askNativeVp9, nativeVp9, resetNativeVp9ForTests } = await import(
  "./nativeVp9.ts"
);

function browserAnswering(
  reply: (codec: string) => Promise<{ supported?: boolean }>,
): void {
  resetNativeVp9ForTests();
  globals.VideoDecoder = { isConfigSupported: ({ codec }) => reply(codec) };
}

test("a browser that decodes profile 1 decodes the stream itself", async () => {
  const asked: string[] = [];
  browserAnswering(async (codec) => {
    asked.push(codec);
    return { supported: true };
  });
  assert.equal(await askNativeVp9(), true);
  assert.equal(nativeVp9(), true);
  // Profile 1, 4:4:4, with every colour field spelled out — the shape the gateway
  // announces, so the question is about the stream that will actually arrive.
  assert.deepEqual(asked, ["vp09.01.40.08.03.06.06.06.00"]);
});

test("a browser without profile 1 leaves the stream to the page", async () => {
  browserAnswering(async () => ({ supported: false }));
  assert.equal(await askNativeVp9(), false);
  assert.equal(nativeVp9(), false);
});

test("a browser that cannot answer keeps its own decoder, whose refusal is its to make", async () => {
  browserAnswering(async () => {
    throw new TypeError("isConfigSupported disliked the string");
  });
  assert.equal(await askNativeVp9(), true);
  // And an answer with no verdict at all is not a "no".
  browserAnswering(async () => ({}));
  assert.equal(await askNativeVp9(), true);
});

test("the question is asked once, and the answer is not available before it", async () => {
  let asked = 0;
  browserAnswering(async () => {
    asked += 1;
    return { supported: false };
  });
  assert.throws(() => nativeVp9(), /before askNativeVp9/);
  assert.equal(await askNativeVp9(), false);
  assert.equal(await askNativeVp9(), false);
  assert.equal(asked, 1);
});
