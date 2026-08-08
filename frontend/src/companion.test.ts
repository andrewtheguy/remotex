// The half of the companion seam no browser test can reach.
//
// `tests/playwright` cannot load an unpacked extension in the harness it has, and
// CLAUDE.md rules out asserting on the things an installed one would show — a toolbar
// icon is pixels, a clipboard poll is timing. What is left, and what actually decides
// whether the client behaves, is the handshake state machine and the three guards on
// the message bus. Both are pure and both are here.
//
// The fakes go in before the import because the module listens and posts at load: in a
// real page the content script may have posted its `hello` before React has rendered
// anything, so an effect would have been too late.
import assert from "node:assert/strict";
import { test } from "node:test";

const ORIGIN = "https://gateway.example";

type Listener = (event: unknown) => void;

const posted: { data: unknown; targetOrigin: unknown }[] = [];
const listeners = new Map<string, Listener[]>();

const fakeWindow = {
  location: { origin: ORIGIN },
  addEventListener(type: string, fn: Listener) {
    const existing = listeners.get(type) ?? [];
    existing.push(fn);
    listeners.set(type, existing);
  },
  removeEventListener(type: string, fn: Listener) {
    const existing = listeners.get(type) ?? [];
    listeners.set(
      type,
      existing.filter((entry) => entry !== fn),
    );
  },
  postMessage(data: unknown, targetOrigin: unknown) {
    posted.push({ data, targetOrigin });
  },
};

(globalThis as unknown as { window: unknown }).window = fakeWindow;

function deliver(event: {
  source?: unknown;
  origin?: string;
  data: unknown;
}): void {
  for (const listener of listeners.get("message") ?? []) {
    listener({
      source: event.source ?? fakeWindow,
      origin: event.origin ?? ORIGIN,
      data: event.data,
    });
  }
}

function helloFromExtension(clipboard = true) {
  return {
    source: "remotex-ext",
    type: "hello",
    version: "1.2.3",
    capabilities: { clipboard, resize: true },
  };
}

const { postToCompanion } = await import("./companion.ts");
const { isExtMessage, isPageMessage, describeRemoteSize } = await import(
  "./companion.contract.ts"
);

test("the page says hello at load, to its own origin", () => {
  assert.equal(posted.length, 1);
  assert.deepEqual(posted[0], {
    data: { source: "remotex-page", type: "hello", client: "remotex" },
    // Never "*": the whole point of the tag-plus-origin pair is that a page embedded
    // somewhere it did not expect cannot be talked to by the frame around it.
    targetOrigin: ORIGIN,
  });
});

test("nothing is posted while the phase is still probing", () => {
  posted.length = 0;
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    false,
  );
  assert.equal(posted.length, 0);
});

test("a message from another window is ignored", () => {
  deliver({ source: { not: "window" }, data: helloFromExtension() });
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    false,
  );
});

test("a message from another origin is ignored", () => {
  deliver({ origin: "https://evil.example", data: helloFromExtension() });
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    false,
  );
});

test("an untagged message is ignored", () => {
  deliver({ data: { type: "hello", version: "1.2.3", capabilities: {} } });
  // The page's own echo must not connect it to itself, either.
  deliver({
    data: { source: "remotex-page", type: "hello", client: "remotex" },
  });
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    false,
  );
});

test("a well-formed hello connects, and events then go out", () => {
  posted.length = 0;
  deliver({ data: helloFromExtension() });

  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "hi" }),
    true,
  );
  assert.deepEqual(posted, [
    {
      data: { source: "remotex-page", type: "clipboardFromRemote", text: "hi" },
      targetOrigin: ORIGIN,
    },
  ]);
});

test("bye stands the seam down, and a later hello brings it back", () => {
  deliver({ data: { source: "remotex-ext", type: "bye" } });
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    false,
  );

  deliver({ data: helloFromExtension() });
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    true,
  );
});

test("the guards accept what they should and refuse the rest", () => {
  assert.equal(isExtMessage(helloFromExtension()), true);
  assert.equal(isExtMessage({ source: "remotex-ext", type: "bye" }), true);

  assert.equal(isExtMessage(null), false);
  assert.equal(isExtMessage("remotex-ext"), false);
  assert.equal(isExtMessage(undefined), false);
  assert.equal(isExtMessage({ type: "bye" }), false);
  assert.equal(isExtMessage({ source: "remotex-page", type: "hello" }), false);
  assert.equal(isExtMessage({ source: "remotex-ext", type: "evict" }), false);
  // A tag on a nested object is not a tag on the message.
  assert.equal(
    isExtMessage({ payload: { source: "remotex-ext" }, type: "bye" }),
    false,
  );

  assert.equal(isPageMessage({ source: "remotex-page", type: "state" }), true);
  assert.equal(isPageMessage({ source: "remotex-ext", type: "hello" }), false);
});

test("a framebuffer is described in the desktop's own points", () => {
  assert.equal(describeRemoteSize(null), "—");
  assert.equal(
    describeRemoteSize({ w: 1920, h: 1080, scale: 1 }),
    "1920 × 1080",
  );
  // The case the whole `scale` field exists for: a Retina Mac's 3840×2160 backing
  // store is a 1920×1080 desktop, not a 4K one.
  assert.equal(
    describeRemoteSize({ w: 3840, h: 2160, scale: 2 }),
    "1920 × 1080 @2x",
  );
  assert.equal(describeRemoteSize({ w: 800, h: 600, scale: 0 }), "800 × 600");
});
