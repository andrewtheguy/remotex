// What a spec makes the browser's own video decoder say and do, so a decision
// that rests on it can be seen on a machine whose browser would answer otherwise.
// The page asks its browser once, at load, whether its own decoder takes a
// Mac's HEVC, which it decodes itself where the answer is no
// (frontend/src/appleMedia.ts), and the paint worker builds the browser's
// decoder where the answer was yes.
import type { Page } from "@playwright/test";

/// The streams a page asks about, as the configuration strings that name them.
const CODECS = {
  /// A High Performance Mac's HEVC.
  hevc: /^(hev1|hvc1)\./,
};

export type Codec = keyof typeof CODECS;

/// The paint worker's script, as the build names it (desktopPainter.ts).
const PAINT_WORKER = "**/desktopPainter.worker-*.js";

/// Put ahead of the paint worker's script: a `VideoDecoder` that takes the
/// streams `pattern` matches and outputs a grey picture for each unit, as many as
/// the stream's own decoder would, and builds the browser's own decoder for
/// anything else. The paint worker uses `state`, `configure`, `decode` and `close`
/// and reads no size off a picture.
function standInDecoder(pattern: RegExp): string {
  return `(() => {
  const Real = globalThis.VideoDecoder;
  const faked = (codec) => new RegExp(${JSON.stringify(pattern.source)}).test(codec);
  globalThis.VideoDecoder = class {
    #init;
    #real = null;
    #state = "unconfigured";
    #picture = null;
    constructor(init) {
      this.#init = init;
    }
    get state() {
      return this.#real ? this.#real.state : this.#state;
    }
    configure(config) {
      if (!faked(config.codec)) {
        this.#real = new Real(this.#init);
        this.#real.configure(config);
        return;
      }
      this.#picture = new OffscreenCanvas(64, 64);
      const context = this.#picture.getContext("2d");
      context.fillStyle = "#808080";
      context.fillRect(0, 0, 64, 64);
      this.#state = "configured";
    }
    decode(chunk) {
      if (this.#real) {
        this.#real.decode(chunk);
        return;
      }
      const frame = new VideoFrame(this.#picture, { timestamp: chunk.timestamp });
      queueMicrotask(() => {
        if (this.#state === "configured") {
          this.#init.output(frame);
        } else {
          frame.close();
        }
      });
    }
    close() {
      this.#real?.close();
      this.#state = "closed";
    }
  };
})();
`;
}

/// Make this page's browser one whose own decoder does or does not take
/// `codec`'s stream: its answer to the page's one question about it, and, where
/// it takes it, a stand-in for that decoder in the paint worker, which builds it,
/// so a browser that has none passes too.
export async function fakeOwnDecoder(
  page: Page,
  codec: Codec,
  takes: boolean,
): Promise<void> {
  const pattern = CODECS[codec];
  await page.addInitScript(
    ({ source, takes }) => {
      const asked = new RegExp(source);
      const ask = VideoDecoder.isConfigSupported.bind(VideoDecoder);
      VideoDecoder.isConfigSupported = (config) =>
        asked.test(config.codec)
          ? Promise.resolve({ supported: takes, config })
          : ask(config);
    },
    { source: pattern.source, takes },
  );
  if (!takes) {
    return;
  }
  await page.context().route(PAINT_WORKER, async (route) => {
    const response = await route.fetch();
    await route.fulfill({
      response,
      body: standInDecoder(pattern) + (await response.text()),
    });
  });
}
