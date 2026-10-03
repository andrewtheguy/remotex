// What the page decodes of a High Performance Mac's own stream. The picture is passed
// only on a definite "yes" from its probe. The sound is always the Mac's own, played
// in whichever form of its configuration this browser both says it takes and takes.
import assert from "node:assert/strict";
import { test } from "node:test";

type Behaviour = "sound" | "error" | "silent" | "unsupported";

interface FakeInit {
  output: (data: { close: () => void }) => void;
  error: (e: unknown) => void;
}

const globals = globalThis as unknown as {
  VideoDecoder: {
    isConfigSupported: (config: {
      codec: string;
    }) => Promise<{ supported?: boolean }>;
  };
  AudioDecoder: unknown;
  EncodedAudioChunk: unknown;
};

const {
  appleEldConfig,
  appleHevcDecoder,
  appleSoundProbed,
  chooseAppleMedia,
  decodesAppleMedia,
  esDescriptor,
  resetAppleMediaForTests,
} = await import("./appleMedia.ts");

/** Both questions answered: the picture's, and the sound's it does not wait for. */
async function choose(): Promise<boolean> {
  const picture = await chooseAppleMedia();
  await appleSoundProbed();
  return picture;
}

const hex = (bytes: Uint8Array) =>
  Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

/** The Mac's AudioSpecificConfig, and the ES_Descriptor Safari decoded it inside. */
const CONFIG = "f8e65000";
const DESCRIPTOR = "03180001000413401500180000000000000000000504f8e65000";

/**
 * A browser whose picture probe answers `picture`, and whose `AudioDecoder` does
 * what `sound` says for each description it is configured with. Returns what the
 * page asked, in order.
 */
function browser(
  picture: () => Promise<{ supported?: boolean }>,
  sound: (description: string) => Behaviour,
  timeoutMs = 2000,
): {
  probes: string[];
  asked: string[];
  tried: { codec: string; description: string }[];
} {
  resetAppleMediaForTests(timeoutMs);
  const asked = {
    probes: [] as string[],
    asked: [] as string[],
    tried: [] as { codec: string; description: string }[],
  };
  globals.VideoDecoder = {
    isConfigSupported: ({ codec }) => {
      asked.probes.push(codec);
      return picture();
    },
  };
  globals.EncodedAudioChunk = class {
    constructor(init: object) {
      Object.assign(this, init);
    }
  };
  globals.AudioDecoder = class {
    static async isConfigSupported(config: { description: Uint8Array }) {
      const description = hex(config.description);
      asked.asked.push(description);
      return { supported: sound(description) !== "unsupported" };
    }
    state = "unconfigured";
    behaviour: Behaviour = "silent";
    init: FakeInit;
    constructor(init: FakeInit) {
      this.init = init;
    }
    configure(config: { codec: string; description: Uint8Array }) {
      const description = hex(config.description);
      asked.tried.push({ codec: config.codec, description });
      this.behaviour = sound(description);
      this.state = "configured";
    }
    decode() {}
    flush(): Promise<void> {
      if (this.behaviour === "sound") {
        this.init.output({ close() {} });
        return Promise.resolve();
      }
      if (this.behaviour === "error") {
        this.init.error(new Error("decoding failed"));
        return Promise.reject(new Error("decoding failed"));
      }
      return new Promise(() => {});
    }
    close() {
      this.state = "closed";
    }
  };
  return asked;
}

const yes = async () => ({ supported: true });

test("the ES_Descriptor is the one Safari decoded the Mac's sound inside", () => {
  assert.equal(
    hex(esDescriptor(Uint8Array.of(0xf8, 0xe6, 0x50, 0x00))),
    DESCRIPTOR,
  );
});

test("a browser that decodes the bare configuration, as Chrome does, is played with it", async () => {
  const asked = browser(yes, (description) =>
    description === CONFIG ? "sound" : "error",
  );
  assert.equal(await choose(), true);
  assert.equal(decodesAppleMedia(), true);
  // The configuration macwork's stream announces: Range Extensions 4:4:4.
  assert.deepEqual(asked.probes, ["hev1.4.10.L150.BE.8"]);
  assert.deepEqual(asked.tried, [{ codec: "mp4a.40.2", description: CONFIG }]);
  const config = appleEldConfig({
    sampleRate: 48_000,
    channels: 2,
    head: Uint8Array.of(0xf8, 0xe6, 0x50, 0x00),
  });
  assert.equal(config.codec, "mp4a.40.2");
  assert.equal(hex(config.description as Uint8Array), CONFIG);
});

test("a browser that decodes only the ES_Descriptor, as Safari does, is played with that", async () => {
  const asked = browser(yes, (description) =>
    description === DESCRIPTOR ? "sound" : "error",
  );
  assert.equal(await choose(), true);
  assert.deepEqual(
    asked.tried.map((t) => t.description),
    [CONFIG, DESCRIPTOR],
  );
  const config = appleEldConfig({
    sampleRate: 48_000,
    channels: 2,
    head: Uint8Array.of(0xf8, 0xe6, 0x50, 0x00),
  });
  assert.equal(hex(config.description as Uint8Array), DESCRIPTOR);
});

test("only a form the browser says it takes is decoded", async () => {
  const asked = browser(yes, (description) =>
    description === CONFIG ? "unsupported" : "sound",
  );
  assert.equal(await choose(), true);
  assert.deepEqual(asked.asked, [CONFIG, DESCRIPTOR]);
  assert.deepEqual(
    asked.tried.map((t) => t.description),
    [DESCRIPTOR],
  );

  const refused = browser(yes, () => "unsupported");
  assert.equal(await choose(), true, "the picture is its own question");
  assert.deepEqual(refused.tried, [], "a form it refuses is never decoded");
});

test("the picture and the sound are answered apart", async () => {
  // The picture passes to a browser that cannot play the sound, which says why.
  browser(yes, () => "error");
  assert.equal(await choose(), true);
  assert.throws(
    () =>
      appleEldConfig({
        sampleRate: 48_000,
        channels: 2,
        head: new Uint8Array(4),
      }),
    /does not decode the Mac's AAC-ELD/,
  );

  // And the sound plays in one sent VP9 for its picture.
  const asked = browser(
    async () => ({ supported: false }),
    () => "sound",
  );
  assert.equal(await choose(), false);
  assert.deepEqual(asked.tried, [{ codec: "mp4a.40.2", description: CONFIG }]);
  const config = appleEldConfig({
    sampleRate: 48_000,
    channels: 2,
    head: Uint8Array.of(0xf8, 0xe6, 0x50, 0x00),
  });
  assert.equal(hex(config.description as Uint8Array), CONFIG);
});

test("anything but a definite yes keeps VP9", async () => {
  browser(
    async () => {
      throw new TypeError("isConfigSupported disliked the string");
    },
    () => "sound",
  );
  assert.equal(await choose(), false);
  browser(
    async () => ({}),
    () => "sound",
  );
  assert.equal(await choose(), false);
  // A sound decoder that never answers is a no, once the attempt runs out of time.
  browser(yes, () => "silent", 10);
  assert.equal(await choose(), true);
  assert.throws(() =>
    appleEldConfig({
      sampleRate: 48_000,
      channels: 2,
      head: new Uint8Array(4),
    }),
  );
});

test("the page does not mount behind a sound decoder that never answers", async () => {
  browser(yes, () => "silent", 50);
  assert.equal(await chooseAppleMedia(), true);
  assert.equal(decodesAppleMedia(), true);
  const probed = appleSoundProbed();
  assert.notEqual(probed, null, "the sound's question is still out");
  assert.throws(() =>
    appleEldConfig({
      sampleRate: 48_000,
      channels: 2,
      head: new Uint8Array(4),
    }),
  );
  await probed;
  assert.equal(appleSoundProbed(), null);
});

test("the question is asked once, and the answer is not available before it", async () => {
  const asked = browser(yes, () => "sound");
  assert.throws(() => decodesAppleMedia());
  await choose();
  await choose();
  assert.equal(asked.probes.length, 1);
  assert.equal(asked.tried.length, 1);
});

/**
 * BETA: a page that can run the software HEVC decoder — cross-origin
 * isolated, with a WebGL 2 canvas that takes the Mac's primaries to present its
 * pictures on — or, with `isolated` false, one that cannot. `webgl` is that
 * canvas's context: whole, without a color space to set, or none. Returns the undo.
 */
function softwareDecoderPage(
  isolated: boolean,
  search = "",
  served: boolean | "never" = true,
  webgl: "whole" | "srgb only" | "none" = "whole",
): () => void {
  const scope = globalThis as unknown as Record<string, unknown>;
  const saved = [
    "crossOriginIsolated",
    "OffscreenCanvas",
    "location",
    "fetch",
  ].map((key) => [key, Object.getOwnPropertyDescriptor(scope, key)] as const);
  Object.defineProperty(scope, "crossOriginIsolated", {
    value: isolated,
    configurable: true,
  });
  // The gateway, asked for the decoder: holding its archive or not.
  scope.fetch = async (url: string, init?: RequestInit) => {
    assert.equal(url, "/hevc/hevc.wasm");
    assert.equal(init?.method, "HEAD");
    if (served === "never") {
      const signal = init?.signal;
      return new Promise((_, reject) => {
        signal?.addEventListener("abort", () => reject(signal.reason));
      });
    }
    return { ok: served };
  };
  scope.OffscreenCanvas = class {
    getContext(kind: string) {
      assert.equal(kind, "webgl2");
      if (webgl === "none") {
        return null;
      }
      return {
        isContextLost: () => false,
        getExtension: () => null,
        ...(webgl === "whole" ? { drawingBufferColorSpace: "srgb" } : {}),
      };
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

const no = async () => ({ supported: false });

test("a picture the browser's decoder refuses is decoded in software on an isolated page", async () => {
  const undo = softwareDecoderPage(true);
  try {
    browser(no, () => "sound");
    assert.equal(await choose(), true);
    assert.equal(appleHevcDecoder(), "software");

    browser(yes, () => "sound");
    assert.equal(await choose(), true);
    assert.equal(appleHevcDecoder(), "native", "the browser's own comes first");

    browser(no, () => "error");
    assert.equal(await choose(), true);
    assert.equal(appleHevcDecoder(), "software", "whatever the sound's answer");
  } finally {
    undo();
  }
});

test("a page that is not cross-origin isolated has no software decoder", async () => {
  const undo = softwareDecoderPage(false);
  try {
    browser(no, () => "sound");
    assert.equal(await choose(), false);
    assert.equal(appleHevcDecoder(), null);
  } finally {
    undo();
  }
});

test("an isolated page whose gateway serves no decoder has no software decoder", async () => {
  const undo = softwareDecoderPage(true, "", false);
  try {
    browser(no, () => "sound");
    assert.equal(await choose(), false);
    assert.equal(appleHevcDecoder(), null);
  } finally {
    undo();
  }
});

test("a page that cannot present the software decoder's pictures has no software decoder", async () => {
  // Said yes, it would be passed a stream whose first picture it could not show.
  for (const webgl of ["none", "srgb only"] as const) {
    const undo = softwareDecoderPage(true, "", true, webgl);
    try {
      browser(no, () => "sound");
      assert.equal(await choose(), false, webgl);
      assert.equal(appleHevcDecoder(), null, webgl);
    } finally {
      undo();
    }
  }
});

test("a gateway that never answers for the decoder is read as serving none", async () => {
  const undo = softwareDecoderPage(true, "", "never");
  try {
    browser(no, () => "sound", 20);
    assert.equal(await choose(), false);
    assert.equal(appleHevcDecoder(), null);
  } finally {
    undo();
  }
});

test("?hevc_decoder=software takes the software decoder without asking the browser's", async () => {
  const undo = softwareDecoderPage(true, "?hevc_decoder=software");
  try {
    const asked = browser(yes, () => "sound");
    assert.equal(await choose(), true);
    assert.equal(appleHevcDecoder(), "software");
    assert.deepEqual(asked.probes, []);
  } finally {
    undo();
  }
  const refused = softwareDecoderPage(false, "?hevc_decoder=software");
  try {
    browser(yes, () => "sound");
    assert.equal(
      await choose(),
      false,
      "asked for software where it cannot run: VP9 and Opus, not the browser's own",
    );
  } finally {
    refused();
  }
});
