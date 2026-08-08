// Immersive mode's decisions, without a browser.
//
// What is worth asserting is that full screen alone arms the lock — by either route,
// including the ⌃⌘F the Fullscreen API never hears about — that leaving by any route
// puts the chord table back, and the two failure paths. None of it is reachable from
// `tests/playwright`: headless full screen is exactly the transient state CLAUDE.md
// says not to assert on, and a held Esc is synthetic input.
import assert from "node:assert/strict";
import { test } from "node:test";

type Listener = () => void;

const fullscreenListeners: Listener[] = [];
const mediaListeners: Listener[] = [];
let fullscreenElement: unknown = null;
// The media query's answer, which a browser flips for *both* kinds of full screen —
// the API's and its own. Tracked apart from `fullscreenElement` so a test can produce
// the ⌃⌘F case, which sets only this one.
let displayModeFullscreen = false;
let requestFullscreenRejects = false;
let lockRejects = false;
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
  documentElement: {
    requestFullscreen: async () => {
      if (requestFullscreenRejects) {
        throw new Error("refused");
      }
      fullscreenElement = fakeDocument.documentElement;
      displayModeFullscreen = true;
      // A browser dispatches the event before resolving this promise, and the module
      // leans on that: the arming `enterImmersive` awaits is the one this starts.
      fireFullscreenChange();
    },
  },
  exitFullscreen: async () => {
    fullscreenElement = null;
    displayModeFullscreen = false;
    fireFullscreenChange();
  },
  addEventListener(type: string, fn: Listener) {
    if (type === "fullscreenchange") {
      fullscreenListeners.push(fn);
    }
  },
};

const fakeWindow = {
  matchMedia(query: string) {
    return {
      get matches() {
        return query === "(display-mode: fullscreen)" && displayModeFullscreen;
      },
      addEventListener(type: string, fn: Listener) {
        if (type === "change") {
          mediaListeners.push(fn);
        }
      },
    };
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
globals.window = fakeWindow;

/// Let the lock's promise chain finish. `setImmediate` runs after every pending
/// microtask, so this is a drain rather than a wait — nothing here is timing.
function settle(): Promise<void> {
  return new Promise((resolve) => {
    setImmediate(resolve);
  });
}

/// The ⌃⌘F / F11 case: full screen the Fullscreen API never hears about, visible only
/// as the media query.
function enterFullscreenOutsideThisModule(): void {
  displayModeFullscreen = true;
  for (const listener of mediaListeners) {
    listener();
  }
}

function leaveFullscreenOutsideThisModule(): void {
  fullscreenElement = null;
  displayModeFullscreen = false;
  fireFullscreenChange();
  for (const listener of mediaListeners) {
    listener();
  }
}

const {
  available,
  DEFAULT_LOCK_CODES,
  enterImmersive,
  exitImmersive,
  immersiveActive,
  keyboardLockHeld,
  onImmersiveChange,
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
  assert.equal(immersiveActive(), false);

  requestFullscreenRejects = false;
});

test("a refused lock leaves the page fullscreen and unlocked", async () => {
  lockRejects = true;

  assert.equal(await enterImmersive(), false);
  // Fullscreen is a real outcome on its own and is not undone: the caller renders
  // "not locked", which is the truth, rather than a window that snaps back. It is
  // still *active*, which is what leaves the button a way out.
  assert.ok(fakeDocument.fullscreenElement);
  assert.equal(keyboardLockHeld(), false);
  assert.equal(immersiveActive(), true);

  lockRejects = false;
  await exitImmersive();
  assert.equal(immersiveActive(), false);
});

test("entering locks exactly the default codes and notifies", async () => {
  const seen: boolean[] = [];
  const stop = onKeyboardLockChange((value) => seen.push(value));

  assert.equal(await enterImmersive(), true);
  assert.equal(keyboardLockHeld(), true);
  assert.deepEqual(lockedCodes, DEFAULT_LOCK_CODES);
  assert.deepEqual(seen, [true]);

  stop();
  await exitImmersive();
});

test("full screen this module did not start still arms the lock", async () => {
  // ⌃⌘F and F11 never touch `document.fullscreenElement` and never fire
  // `fullscreenchange`, so before the media query was watched this was a full screen
  // window that quietly kept ⌘W. It is the whole reason the button is not the trigger.
  const seen: boolean[] = [];
  const stop = onKeyboardLockChange((value) => seen.push(value));
  const active: boolean[] = [];
  const stopActive = onImmersiveChange((value) => active.push(value));
  lockedCodes = undefined;

  enterFullscreenOutsideThisModule();
  await settle();

  assert.equal(keyboardLockHeld(), true);
  assert.equal(immersiveActive(), true);
  assert.deepEqual(lockedCodes, DEFAULT_LOCK_CODES);
  assert.deepEqual(seen, [true]);
  assert.deepEqual(active, [true]);

  stop();
  stopActive();
  leaveFullscreenOutsideThisModule();
});

test("one transition reported twice arms one lock", async () => {
  // `requestFullscreen` fires `fullscreenchange` *and* flips the media query, so the
  // handler runs twice for one entry. A second `lock()` would be a second grant to
  // hand back, and the unlock count is what would show it.
  const before = unlocks;

  await enterImmersive();
  enterFullscreenOutsideThisModule();
  await settle();

  assert.equal(keyboardLockHeld(), true);
  await exitImmersive();
  assert.equal(unlocks, before + 1);
});

test("leaving fullscreen by any route drops the lock", async () => {
  const seen: boolean[] = [];
  const stop = onKeyboardLockChange((value) => seen.push(value));
  await enterImmersive();
  const before = unlocks;

  // A held Esc, the platform's ⌃⌘F, a window manager: none of them call this module,
  // and all of them produce one of the two signals. It is the only reason the client
  // does not go on believing it still has ⌘W.
  leaveFullscreenOutsideThisModule();

  assert.equal(keyboardLockHeld(), false);
  assert.equal(immersiveActive(), false);
  assert.equal(unlocks, before + 1);
  assert.deepEqual(seen, [true, false]);
  stop();
});

test("a lock granted after fullscreen ended is handed straight back", async () => {
  // A held Esc during the milliseconds `lock()` is in flight. Without the re-read on
  // the way out, the browser has granted a lock that nothing will ever take back.
  const before = unlocks;

  enterFullscreenOutsideThisModule();
  leaveFullscreenOutsideThisModule();
  await settle();

  assert.equal(keyboardLockHeld(), false);
  assert.equal(immersiveActive(), false);
  assert.equal(unlocks, before + 1);
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
  assert.equal(immersiveActive(), false);
});
