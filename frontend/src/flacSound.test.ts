// Whether this browser is offered lossless sound: only where its own decoder,
// configured with the header the page writes, turns one of the gateway's frames
// into a frame of sound.
import assert from "node:assert/strict";
import { test } from "node:test";
import { askFlac, decodesFlac, resetFlacForTests } from "./flacSound.ts";

/** What a fake decoder does with the frame: Chrome's and Firefox's, and Safari's. */
type Behaviour = "decodes" | "fails" | "silent" | "stalls" | "unsupported";

interface FakeInit {
  output: (data: { numberOfFrames: number; close(): void }) => void;
  error: (e: unknown) => void;
}

const scope = globalThis as Record<string, unknown>;

/** The answer for a browser whose `AudioDecoder` does what `behaviour` says. */
async function answerFor(behaviour: Behaviour | null): Promise<boolean> {
  const had = [scope.AudioDecoder, scope.EncodedAudioChunk];
  scope.AudioDecoder =
    behaviour === null
      ? undefined
      : class {
          static isConfigSupported(config: AudioDecoderConfig) {
            assert.equal(config.codec, "flac");
            return behaviour === "stalls"
              ? new Promise(() => {})
              : Promise.resolve({ supported: behaviour !== "unsupported" });
          }
          state = "unconfigured";
          #init: FakeInit;
          constructor(init: FakeInit) {
            this.#init = init;
          }
          configure() {
            this.state = "configured";
          }
          decode() {
            if (behaviour === "decodes") {
              this.#init.output({ numberOfFrames: 960, close: () => {} });
            } else if (behaviour === "fails") {
              this.#init.error(
                new Error("InternalAudioDecoderCocoa decoding failed"),
              );
            }
          }
          flush() {
            return behaviour === "silent"
              ? new Promise(() => {})
              : Promise.resolve();
          }
          close() {
            this.state = "closed";
          }
        };
  scope.EncodedAudioChunk = class {};
  try {
    resetFlacForTests(50);
    await askFlac();
    return decodesFlac();
  } finally {
    [scope.AudioDecoder, scope.EncodedAudioChunk] = had;
  }
}

test("a decoder that decodes the frame is offered lossless sound", async () => {
  assert.equal(await answerFor("decodes"), true);
});

test("one that takes FLAC and then fails the frame is not", async () => {
  // Safari 27, which says yes to the configuration.
  assert.equal(await answerFor("fails"), false);
});

test("one that refuses the configuration, or never answers, is not", async () => {
  assert.equal(await answerFor("unsupported"), false);
  assert.equal(await answerFor("silent"), false);
  // Its support check alone stalling counts against the same time.
  assert.equal(await answerFor("stalls"), false);
});

test("a browser with no audio decoder is not", async () => {
  assert.equal(await answerFor(null), false);
});

test("the answer is read only once it is asked", () => {
  resetFlacForTests();
  assert.throws(decodesFlac, /before askFlac/);
});
