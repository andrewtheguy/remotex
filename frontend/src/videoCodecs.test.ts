// The two questions the page asks about the pictures its own decoder must take, and
// what each answer means. The property under test is that only a definite "yes"
// offers a picture: a "no", a browser that cannot answer, and a browser with no
// `VideoDecoder` at all are offered neither.
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

test("each picture is asked about as it arrives, and answered for", async () => {
  const asked: string[] = [];
  browserAnswering(async (codec) => {
    asked.push(codec);
    return { supported: codec.startsWith("vp09") };
  });
  await askVideoCodecs();
  // wlshare's 4:2:0 VP9 and the Mac's High profile H.264, as the gateway names
  // them.
  assert.deepEqual(asked.sort(), [
    "avc1.640028",
    "vp09.00.40.08.01.06.06.06.00",
  ]);
  assert.equal(decodesVp9(), true);
  assert.equal(decodesAppleH264(), false);
});

test("a browser that cannot answer is offered neither", async () => {
  browserAnswering(async () => {
    throw new TypeError("isConfigSupported disliked the string");
  });
  await askVideoCodecs();
  assert.equal(decodesVp9(), false);
  assert.equal(decodesAppleH264(), false);
  // And an answer with no verdict at all is not a "yes".
  browserAnswering(async () => ({}));
  await askVideoCodecs();
  assert.equal(decodesVp9(), false);
  assert.equal(decodesAppleH264(), false);
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
  assert.equal(asked, 2);
});
