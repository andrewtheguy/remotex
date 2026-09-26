// Whether the page asks for a High Performance Mac's own HEVC. Only a definite "yes"
// does: a "no", a browser that cannot answer, and an answer with no verdict all keep
// the VP9 every browser here decodes.
import assert from "node:assert/strict";
import { test } from "node:test";

const globals = globalThis as unknown as {
  VideoDecoder: {
    isConfigSupported: (config: {
      codec: string;
    }) => Promise<{ supported?: boolean }>;
  };
};

const { chooseAppleHevc, decodesAppleHevc, resetAppleHevcForTests } =
  await import("./appleHevc.ts");

function browserAnswering(
  reply: (codec: string) => Promise<{ supported?: boolean }>,
): void {
  resetAppleHevcForTests();
  globals.VideoDecoder = { isConfigSupported: ({ codec }) => reply(codec) };
}

test("a browser that decodes the Mac's stream says so", async () => {
  const asked: string[] = [];
  browserAnswering(async (codec) => {
    asked.push(codec);
    return { supported: true };
  });
  assert.equal(await chooseAppleHevc(), true);
  assert.equal(decodesAppleHevc(), true);
  // The configuration macwork's stream announces: Range Extensions 4:4:4.
  assert.deepEqual(asked, ["hev1.4.10.L150.BE.8"]);
});

test("anything but a definite yes keeps VP9", async () => {
  browserAnswering(async () => ({ supported: false }));
  assert.equal(await chooseAppleHevc(), false);
  browserAnswering(async () => {
    throw new TypeError("isConfigSupported disliked the string");
  });
  assert.equal(await chooseAppleHevc(), false);
  browserAnswering(async () => ({}));
  assert.equal(await chooseAppleHevc(), false);
});

test("the question is asked once, and the answer is not available before it", async () => {
  let asked = 0;
  browserAnswering(async () => {
    asked += 1;
    return { supported: true };
  });
  assert.throws(() => decodesAppleHevc());
  await chooseAppleHevc();
  await chooseAppleHevc();
  assert.equal(asked, 1);
});
