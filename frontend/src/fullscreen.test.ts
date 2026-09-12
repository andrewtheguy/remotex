// The control, over a fake document. What matters here is which element is asked, that
// asking is a toggle, and that a refusal reaches the caller rather than being dropped —
// the Keyboard Lock that follows is tested in keyboardLock.test.ts.
import assert from "node:assert/strict";
import { test } from "node:test";

type Listener = () => void;

const listeners = new Set<Listener>();
let fullscreenEnabled = true;
let fullscreenElement: unknown = null;
let requests: unknown[] = [];
let exits = 0;
let refusal: Error | null = null;

const documentElement = {
  requestFullscreen(options?: unknown) {
    requests.push(options);
    return refusal ? Promise.reject(refusal) : Promise.resolve();
  },
};

const fakeDocument = {
  documentElement,
  get fullscreenEnabled() {
    return fullscreenEnabled;
  },
  get fullscreenElement() {
    return fullscreenElement;
  },
  exitFullscreen() {
    exits += 1;
    return Promise.resolve();
  },
  addEventListener(type: string, listener: Listener) {
    if (type === "fullscreenchange") {
      listeners.add(listener);
    }
  },
  removeEventListener(type: string, listener: Listener) {
    if (type === "fullscreenchange") {
      listeners.delete(listener);
    }
  },
};

(globalThis as unknown as { document: unknown }).document = fakeDocument;

const {
  fullscreenSupported,
  isFullscreen,
  onFullscreenChange,
  toggleFullscreen,
} = await import("./fullscreen.ts");

test("support is the browser's own answer", () => {
  assert.equal(fullscreenSupported(), true);
  fullscreenEnabled = false;
  assert.equal(fullscreenSupported(), false);
  fullscreenEnabled = true;
});

test("only an element full screen counts", () => {
  assert.equal(isFullscreen(), false);
  fullscreenElement = documentElement;
  assert.equal(isFullscreen(), true);
  fullscreenElement = null;
});

test("entering asks the document element, with no navigation UI", async () => {
  requests = [];
  await toggleFullscreen();

  assert.equal(requests.length, 1);
  assert.deepEqual(requests[0], { navigationUI: "hide" });
});

test("the same call leaves once the page is in full screen", async () => {
  requests = [];
  fullscreenElement = documentElement;
  await toggleFullscreen();
  fullscreenElement = null;

  assert.equal(exits, 1);
  assert.equal(requests.length, 0);
});

test("a refused request reaches the caller", async () => {
  refusal = new Error("gesture required");
  await assert.rejects(toggleFullscreen(), /gesture required/);
  refusal = null;
});

test("subscribers are told, and detach", () => {
  let seen = 0;
  const detach = onFullscreenChange(() => {
    seen += 1;
  });
  for (const listener of listeners) {
    listener();
  }
  assert.equal(seen, 1);

  detach();
  for (const listener of listeners) {
    listener();
  }
  assert.equal(seen, 1);
});
