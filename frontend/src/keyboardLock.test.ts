// The lock is a passive browser enhancement. These tests cover its fullscreen
// boundary without browser timing or UI: the fixed Command table is tested separately
// in macKeys.test.ts.
import assert from "node:assert/strict";
import { test } from "node:test";

type Listener = () => void;

const fullscreenListeners: Listener[] = [];
const mediaListeners: Listener[] = [];
let fullscreenElement: unknown = null;
let displayModeFullscreen = false;
let lockCalls = 0;
let unlocks = 0;
let lockedCodes: readonly string[] | undefined;

function fireFullscreenChange(): void {
  for (const listener of fullscreenListeners) {
    listener();
  }
}

const fakeDocument = {
  get fullscreenElement() {
    return fullscreenElement;
  },
  addEventListener(type: string, listener: Listener) {
    if (type === "fullscreenchange") {
      fullscreenListeners.push(listener);
    }
  },
};

const fakeWindow = {
  matchMedia(query: string) {
    return {
      get matches() {
        return query === "(display-mode: fullscreen)" && displayModeFullscreen;
      },
      addEventListener(type: string, listener: Listener) {
        if (type === "change") {
          mediaListeners.push(listener);
        }
      },
    };
  },
};

const fakeNavigator = {
  keyboard: {
    lock: async (codes?: readonly string[]) => {
      lockCalls += 1;
      lockedCodes = codes;
    },
    unlock: () => {
      unlocks += 1;
    },
  },
};

const globals = globalThis as unknown as {
  document: unknown;
  navigator: unknown;
  window: unknown;
};
globals.document = fakeDocument;
globals.navigator = fakeNavigator;
globals.window = fakeWindow;

function settle(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

function enterFullscreen(): void {
  fullscreenElement = fakeDocument;
  displayModeFullscreen = true;
  fireFullscreenChange();
  for (const listener of mediaListeners) {
    listener();
  }
}

function leaveFullscreen(): void {
  fullscreenElement = null;
  displayModeFullscreen = false;
  fireFullscreenChange();
  for (const listener of mediaListeners) {
    listener();
  }
}

const { DEFAULT_LOCK_CODES } = await import("./keyboardLock.ts");
const { BROWSER_RESERVED_CHORD_CODES } = await import("./macKeys.ts");

test("windowed startup does not ask for a lock", () => {
  assert.equal(lockCalls, 0);
});

test("the lock covers every browser-reserved chord plus Command-Q", () => {
  assert.deepEqual(
    [...DEFAULT_LOCK_CODES].sort(),
    [...BROWSER_RESERVED_CHORD_CODES, "KeyQ"].sort(),
  );
});

test("fullscreen automatically takes one lock even when reported twice", async () => {
  enterFullscreen();
  await settle();

  assert.equal(lockCalls, 1);
  assert.deepEqual(lockedCodes, DEFAULT_LOCK_CODES);
});

test("leaving fullscreen releases the automatic lock", () => {
  const before = unlocks;
  leaveFullscreen();
  assert.equal(unlocks, before + 1);
});
