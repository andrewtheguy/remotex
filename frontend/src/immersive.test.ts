// Immersive mode's decisions, without a browser.
//
// What is worth asserting is the pair of failure paths and the fact that leaving
// fullscreen — by any route, including the held Esc nothing can capture — puts the
// chord table back. Neither is reachable from `tests/playwright`: headless fullscreen
// is exactly the transient state CLAUDE.md says not to assert on, and a held Esc is
// synthetic input.
import assert from "node:assert/strict";
import { test } from "node:test";

type Listener = () => void;

const fullscreenListeners: Listener[] = [];
let fullscreenElement: unknown = null;
let requestFullscreenRejects = false;
let lockRejects = false;
let unlocks = 0;
let lockedCodes: readonly string[] | undefined;

const fakeDocument = {
  get fullscreenElement() {
    return fullscreenElement;
  },
  documentElement: {
    requestFullscreen: async () => {
      if (requestFullscreenRejects) {
        throw new Error("refused");
      }
      fullscreenElement = fakeDocument.documentElement;
    },
  },
  exitFullscreen: async () => {
    fullscreenElement = null;
  },
  addEventListener(type: string, fn: Listener) {
    if (type === "fullscreenchange") {
      fullscreenListeners.push(fn);
    }
  },
};

const fakeNavigator = {
  keyboard: {
    lock: async (codes?: readonly string[]) => {
      if (lockRejects) {
        throw new Error("refused");
      }
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
globals.window = { isSecureContext: true };

function leaveFullscreenOutsideThisModule(): void {
  fullscreenElement = null;
  for (const listener of fullscreenListeners) {
    listener();
  }
}

const {
  available,
  DEFAULT_LOCK_CODES,
  enterImmersive,
  exitImmersive,
  keyboardLockHeld,
  onKeyboardLockChange,
} = await import("./immersive.ts");
const { BROWSER_RESERVED_CHORD_CODES } = await import("./macKeys.ts");

test("the lock list and the chord table are the same six, plus Q", () => {
  // Not a restatement of the constant: this is the invariant that a chord the table
  // promises to translate is a chord the browser has agreed not to eat. Q is the one
  // addition — unmapped, but locked, or ⌘Q ends the browser mid-session.
  for (const code of BROWSER_RESERVED_CHORD_CODES) {
    assert.ok(
      DEFAULT_LOCK_CODES.includes(code),
      `${code} is translated but not locked`,
    );
  }
  assert.deepEqual([...DEFAULT_LOCK_CODES].sort(), [
    "KeyL",
    "KeyN",
    "KeyO",
    "KeyQ",
    "KeyR",
    "KeyT",
    "KeyW",
  ]);
});

test("available asks for the API and nothing else", () => {
  assert.equal(available(), true);
});

test("a refused fullscreen never reaches the lock", async () => {
  requestFullscreenRejects = true;
  lockedCodes = undefined;

  assert.equal(await enterImmersive(), false);
  assert.equal(lockedCodes, undefined);
  assert.equal(keyboardLockHeld(), false);

  requestFullscreenRejects = false;
});

test("a refused lock leaves the page fullscreen and unlocked", async () => {
  lockRejects = true;

  assert.equal(await enterImmersive(), false);
  // Fullscreen is a real outcome on its own and is not undone: the caller renders
  // "not locked", which is the truth, rather than a window that snaps back.
  assert.ok(fakeDocument.fullscreenElement);
  assert.equal(keyboardLockHeld(), false);

  lockRejects = false;
  await exitImmersive();
});

test("entering locks exactly the requested codes and notifies", async () => {
  const seen: boolean[] = [];
  const stop = onKeyboardLockChange((value) => seen.push(value));

  assert.equal(await enterImmersive(), true);
  assert.equal(keyboardLockHeld(), true);
  assert.deepEqual(lockedCodes, DEFAULT_LOCK_CODES);
  assert.deepEqual(seen, [true]);

  stop();
  await exitImmersive();
});

test("leaving fullscreen by any route drops the lock", async () => {
  const seen: boolean[] = [];
  const stop = onKeyboardLockChange((value) => seen.push(value));
  await enterImmersive();
  const before = unlocks;

  // A held Esc, the platform's ⌃⌘F, a window manager: none of them call this module,
  // and all of them produce this event. It is the only reason the client does not go
  // on believing it still has ⌘W.
  leaveFullscreenOutsideThisModule();

  assert.equal(keyboardLockHeld(), false);
  assert.equal(unlocks, before + 1);
  assert.deepEqual(seen, [true, false]);
  stop();
});

test("exiting deliberately does not unlock twice", async () => {
  await enterImmersive();
  const before = unlocks;

  await exitImmersive();

  // `exitImmersive` unlocks and clears the flag *before* leaving fullscreen, so the
  // `fullscreenchange` it causes finds nothing held and does not unlock again. Get
  // that order wrong and every deliberate exit unlocks a lock that is already gone.
  assert.equal(unlocks, before + 1);
  assert.equal(keyboardLockHeld(), false);
  assert.equal(fakeDocument.fullscreenElement, null);
});
