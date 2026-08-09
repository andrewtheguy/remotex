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

// An app window, because that is the only place the seam exists at all — in a tab the
// module posts nothing and settles to `absent` at once. That half cannot have a test
// beside this one: the window mode is read at import, and bun shares one module
// registry across a run, so the first file to import decides for every other. What is
// testable of it is the decision, in appWindow.test.ts.
const fakeWindow = {
  location: { origin: ORIGIN },
  matchMedia(query: string) {
    return { matches: query.includes("standalone") };
  },
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

function fire(type: string): void {
  for (const listener of listeners.get(type) ?? []) {
    listener({ type });
  }
}

/** Real time, because the deadline is the thing under test rather than a guess. */
function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function helloFromExtension(clipboard = true) {
  return {
    source: "remotex-ext",
    type: "hello",
    version: "1.2.3",
    capabilities: { clipboard, resize: true },
  };
}

const { companionPhase, HANDSHAKE_DEADLINE_MS, postToCompanion } = await import(
  "./companion.ts"
);
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

// From here the order of these tests is load-bearing. There is one module instance for
// the whole file and one deadline, armed at load, so everything that needs a seam that
// has never connected has to run before the hello that connects it.

test("silence settles to absent once the deadline passes", async () => {
  // A real wait, and the deadline is the thing under test rather than a machine-speed
  // guess: it was armed at module load, and what is asserted is the far side of it.
  await delay(HANDSHAKE_DEADLINE_MS + 100);
  posted.length = 0;

  assert.equal(companionPhase(), "absent");
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    false,
  );
});

test("a bfcache restore asks again, from a seam that had stood down", () => {
  // A restore replays no content-script injection, so `absent` is no longer an answer
  // this page is entitled to keep: the extension may have come back with the page, or
  // may have been removed while it was away.
  posted.length = 0;

  fire("pageshow");
  assert.deepEqual(
    posted.map((entry) => entry.data),
    [{ source: "remotex-page", type: "hello", client: "remotex" }],
  );
  // Back to probing, not left at absent, and the deadline goes out with the hello —
  // which is what stops a restore that nobody answers from waiting for ever.
  assert.equal(companionPhase(), "probing");
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    false,
  );
});

test("a second restore re-arms the deadline instead of adding one", async () => {
  // Two restores inside one deadline. The older probe's timer comes due first and finds
  // a `probing` that belongs to the newer one; without the generation check it would
  // call that absent early, cutting the new question's answer short by exactly the gap
  // between the two — and a premature `absent` starts the focus-driven clipboard reader,
  // which is the flap the three phases exist to prevent.
  const gap = 200;
  fire("pageshow");
  await delay(gap);
  fire("pageshow");

  // Past the first probe's deadline, well short of the second's.
  await delay(HANDSHAKE_DEADLINE_MS - gap + 100);
  assert.equal(companionPhase(), "probing");

  // And the newer one still settles on its own schedule, so nothing waits for ever.
  await delay(gap + 100);
  assert.equal(companionPhase(), "absent");
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

test("a restore of a connected seam says hello without re-opening the question", () => {
  posted.length = 0;

  fire("pageshow");
  assert.deepEqual(
    posted.map((entry) => entry.data),
    [{ source: "remotex-page", type: "hello", client: "remotex" }],
  );
  // Still connected, so the clipboard never stands down for a second and a half over a
  // question that has already been answered.
  assert.equal(companionPhase(), "connected");
  assert.equal(
    postToCompanion({ type: "clipboardFromRemote", text: "x" }),
    true,
  );
});

test("the guards accept what they should and refuse the rest", () => {
  assert.equal(isExtMessage(helloFromExtension()), true);
  assert.equal(
    isExtMessage({ source: "remotex-ext", type: "clipboardLocal", text: "x" }),
    true,
  );

  assert.equal(isExtMessage(null), false);
  assert.equal(isExtMessage("remotex-ext"), false);
  assert.equal(isExtMessage(undefined), false);
  assert.equal(isExtMessage({ type: "clipboardLocal" }), false);
  assert.equal(isExtMessage({ source: "remotex-page", type: "hello" }), false);
  assert.equal(isExtMessage({ source: "remotex-ext", type: "evict" }), false);
  // The goodbye nothing can send any more: the extension serves one hard-coded host and
  // has no moment where it learns it is leaving.
  assert.equal(isExtMessage({ source: "remotex-ext", type: "bye" }), false);
  // A tag on a nested object is not a tag on the message.
  assert.equal(
    isExtMessage({
      payload: { source: "remotex-ext" },
      type: "clipboardLocal",
    }),
    false,
  );

  assert.equal(isPageMessage({ source: "remotex-page", type: "state" }), true);
  assert.equal(isPageMessage({ source: "remotex-ext", type: "hello" }), false);
  // The extension's own commands are not requests it may send itself back.
  assert.equal(
    isPageMessage({ source: "remotex-page", type: "clipboardLocal" }),
    false,
  );
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
