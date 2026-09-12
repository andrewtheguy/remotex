// The lock follows immersive full screen and asks for everything. These tests cover
// that boundary without browser timing or UI: the fixed Command table is tested
// separately in macKeys.test.ts, and the control itself in fullscreen.test.ts.
import assert from "node:assert/strict";
import { test } from "node:test";

type Listener = () => void;

const fullscreenListeners: Listener[] = [];
// Watched only to assert nothing subscribes to it. The browser's own full screen is
// the one this module must not arm on.
const mediaListeners: Listener[] = [];
let fullscreenElement: unknown = null;
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
  removeEventListener() {},
};

const fakeWindow = {
  matchMedia(_query: string) {
    return {
      matches: true,
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
  fireFullscreenChange();
}

function leaveFullscreen(): void {
  fullscreenElement = null;
  fireFullscreenChange();
}

const { keyboardLockSupported } = await import("./keyboardLock.ts");

test("windowed startup does not ask for a lock", () => {
  assert.equal(lockCalls, 0);
});

test("the browser's own full screen is not a route into the lock", () => {
  // `(display-mode: fullscreen)` matches for every query this fake answers, so a module
  // that watched it would have subscribed by now — and would have armed a lock Chromium
  // never activates, which is the bug this file is the guard for.
  assert.equal(mediaListeners.length, 0);
  assert.equal(lockCalls, 0);
});

test("element full screen takes one lock even when reported twice", async () => {
  enterFullscreen();
  fireFullscreenChange();
  await settle();

  assert.equal(lockCalls, 1);
  // No list: every key Chromium will hand over, the Super key included.
  assert.equal(lockedCodes, undefined);
});

test("leaving full screen releases the lock", () => {
  const before = unlocks;
  leaveFullscreen();
  assert.equal(unlocks, before + 1);
});

test("the API's presence is reported, because the page promises keys on it", () => {
  // Not whether a lock took — that stays unreported by design — but whether this
  // browser has one to take, which is what the menu and the Help card word
  // themselves from.
  assert.equal(keyboardLockSupported(), true);
  const navigator = fakeNavigator as { keyboard?: unknown };
  const api = navigator.keyboard;
  navigator.keyboard = undefined;
  assert.equal(keyboardLockSupported(), false);
  navigator.keyboard = api;
});
