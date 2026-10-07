// The one decoder question the client asks, and what each answer selects. The
// property under test is that only a definite "no" gives up the colour: a browser's
// "yes", and a browser that cannot answer, both ask for 4:4:4. And on a gateway
// that allows the page's own VP9 decoder, a "no" gives it up only where the page
// cannot run that decoder.
import assert from "node:assert/strict";
import { test } from "node:test";

const globals = globalThis as unknown as {
  VideoDecoder: {
    isConfigSupported: (config: {
      codec: string;
    }) => Promise<{ supported?: boolean }>;
  };
};

const {
  chooseVideoChroma,
  resetVideoChromaForTests,
  videoChroma,
  videoDecoder,
} = await import("./videoChroma.ts");

function browserAnswering(
  reply: (codec: string) => Promise<{ supported?: boolean }>,
): void {
  resetVideoChromaForTests();
  globals.VideoDecoder = { isConfigSupported: ({ codec }) => reply(codec) };
}

/**
 * A page that can run the software decoder — cross-origin isolated, with a WebGL 2
 * canvas off the page — or one that cannot, at a URL ending `search`. Returns the
 * undo.
 */
function page(runs: boolean, search = ""): () => void {
  const scope = globalThis as unknown as Record<string, unknown>;
  const saved = ["crossOriginIsolated", "OffscreenCanvas", "location"].map(
    (key) => [key, Object.getOwnPropertyDescriptor(scope, key)] as const,
  );
  Object.defineProperty(scope, "crossOriginIsolated", {
    value: runs,
    configurable: true,
  });
  scope.OffscreenCanvas = class {
    getContext(kind: string) {
      assert.equal(kind, "webgl2");
      // No color space to set: the gateway's VP9 is presented in sRGB's.
      return { isContextLost: () => false, getExtension: () => null };
    }
  };
  Object.defineProperty(scope, "location", {
    value: { search },
    configurable: true,
  });
  return () => {
    for (const [key, descriptor] of saved) {
      if (descriptor) {
        Object.defineProperty(scope, key, descriptor);
      } else {
        delete scope[key];
      }
    }
  };
}

test("a browser that decodes profile 1 asks for 4:4:4", async () => {
  const asked: string[] = [];
  browserAnswering(async (codec) => {
    asked.push(codec);
    return { supported: true };
  });
  assert.deepEqual(await chooseVideoChroma(false), {
    chroma: "444",
    decoder: "native",
  });
  assert.equal(videoChroma(), "444");
  assert.equal(videoDecoder(), "native");
  // Profile 1, 4:4:4, with every colour field spelled out — the shape the gateway
  // announces for a 4:4:4 stream, so the question is about the stream that will
  // actually arrive.
  assert.deepEqual(asked, ["vp09.01.40.08.03.06.06.06.00"]);
});

test("a browser without profile 1 asks for 4:2:0", async () => {
  browserAnswering(async () => ({ supported: false }));
  assert.equal((await chooseVideoChroma(false)).chroma, "420");
  assert.equal(videoChroma(), "420");
  assert.equal(videoDecoder(), "native");
});

test("a browser that cannot answer asks for 4:4:4, since a refusal is the decoder's to make", async () => {
  browserAnswering(async () => {
    throw new TypeError("isConfigSupported disliked the string");
  });
  assert.equal((await chooseVideoChroma(false)).chroma, "444");
  // And an answer with no verdict at all is not a "no".
  browserAnswering(async () => ({}));
  assert.equal((await chooseVideoChroma(false)).chroma, "444");
});

test("the question is asked once, and the answer is not available before it", async () => {
  let asked = 0;
  browserAnswering(async () => {
    asked += 1;
    return { supported: false };
  });
  assert.throws(() => videoChroma(), /before chooseVideoChroma/);
  assert.throws(() => videoDecoder(), /before chooseVideoChroma/);
  assert.equal((await chooseVideoChroma(false)).chroma, "420");
  assert.equal((await chooseVideoChroma(true)).chroma, "420");
  assert.equal(asked, 1);
});

test("what the gateway's key, the browser's answer, the page and its URL select", async () => {
  const SWITCH = "?vp9_decoder=software";
  // The gateway allows it, the browser takes profile 1, the page runs the module,
  // the URL: then the chroma asked for and what decodes it.
  const table: [boolean, boolean, boolean, string, string, string][] = [
    // A gateway that does not allow it: the browser's answer alone, whatever else.
    [false, true, true, "", "444", "native"],
    [false, false, true, "", "420", "native"],
    [false, true, true, SWITCH, "444", "native"],
    [false, false, true, SWITCH, "420", "native"],
    // One that does: the browser's own decoder where it takes profile 1,
    [true, true, true, "", "444", "native"],
    // the module where it does not, which is what the key is for,
    [true, false, true, "", "444", "software"],
    // 4:2:0 where the module cannot run either,
    [true, false, false, "", "420", "native"],
    [true, true, false, "", "444", "native"],
    // and the module over the browser's own for a URL that asks, where it runs.
    [true, true, true, SWITCH, "444", "software"],
    [true, false, true, SWITCH, "444", "software"],
    [true, true, false, SWITCH, "444", "native"],
    [true, false, false, SWITCH, "420", "native"],
  ];
  for (const [allowed, takes, runs, search, chroma, decoder] of table) {
    const undo = page(runs, search);
    try {
      browserAnswering(async () => ({ supported: takes }));
      assert.deepEqual(
        // As main.tsx gives it: the gateway's config, still on its way.
        await chooseVideoChroma(Promise.resolve(allowed)),
        { chroma, decoder },
        JSON.stringify({ allowed, takes, runs, search }),
      );
    } finally {
      undo();
    }
  }
});

test("a browser that cannot answer keeps its own decoder where the module is allowed", async () => {
  const undo = page(true);
  try {
    browserAnswering(async () => {
      throw new TypeError("isConfigSupported disliked the string");
    });
    assert.deepEqual(await chooseVideoChroma(true), {
      chroma: "444",
      decoder: "native",
    });
  } finally {
    undo();
  }
});
