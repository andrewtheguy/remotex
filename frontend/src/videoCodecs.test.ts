// The question the page asks about wlshare's VP9, and what the answer means, beside
// the Mac's H.264, which is not asked. The property under test is that only a
// definite "yes" offers the VP9: a "no", a browser that cannot answer, and a
// browser with no `VideoDecoder` at all are not offered it; and that any
// `VideoDecoder` is offered the H.264, and none is not.
import assert from "node:assert/strict";
import { test } from "node:test";

const globals = globalThis as unknown as {
  VideoDecoder:
    | {
        isConfigSupported: (config: {
          codec: string;
        }) => Promise<{ supported?: boolean }>;
      }
    | undefined;
};

const {
  askVideoCodecs,
  decodesAppleH264,
  decodesVp9,
  resetVideoCodecsForTests,
} = await import("./videoCodecs.ts");

function browserAnswering(
  reply: (codec: string) => Promise<{ supported?: boolean }>,
): void {
  resetVideoCodecsForTests();
  globals.VideoDecoder = { isConfigSupported: ({ codec }) => reply(codec) };
}

test("the VP9 is asked about as it arrives, and the H.264 not at all", async () => {
  const asked: string[] = [];
  browserAnswering(async (codec) => {
    asked.push(codec);
    return { supported: false };
  });
  await askVideoCodecs();
  // wlshare's 4:2:0 VP9, as the gateway names it.
  assert.deepEqual(asked, ["vp09.00.40.08.01.06.06.06.00"]);
  assert.equal(decodesVp9(), false);
  assert.equal(decodesAppleH264(), true, "a VideoDecoder is the answer");
});

test("a browser that cannot answer is not offered the VP9", async () => {
  browserAnswering(async () => {
    throw new TypeError("isConfigSupported disliked the string");
  });
  await askVideoCodecs();
  assert.equal(decodesVp9(), false);
  assert.equal(decodesAppleH264(), true);
  // And an answer with no verdict at all is not a "yes".
  browserAnswering(async () => ({}));
  await askVideoCodecs();
  assert.equal(decodesVp9(), false);
});

test("a browser with no VideoDecoder is offered neither", async () => {
  resetVideoCodecsForTests();
  globals.VideoDecoder = undefined;
  await askVideoCodecs();
  assert.equal(decodesVp9(), false);
  assert.equal(decodesAppleH264(), false);
});

test("the questions are asked once, and not answered before", async () => {
  let asked = 0;
  browserAnswering(async () => {
    asked += 1;
    return { supported: true };
  });
  assert.throws(() => decodesVp9(), /before askVideoCodecs/);
  await askVideoCodecs();
  await askVideoCodecs();
  assert.equal(asked, 1);
});
