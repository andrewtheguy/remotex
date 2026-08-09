// Keyboard Lock is an automatic enhancement for a browser tab, not a user-facing
// mode. The Command translator always uses its complete table; this lock only makes
// the browser-reserved members of that table reach the page when Chromium permits it.
//
// Chromium restricts Keyboard Lock to fullscreen. App windows and `remotex.app` do
// not need it in windowed mode because their hosts reserve no browser shortcuts. A
// normal windowed tab keeps those shortcuts before any page code can see them.
//
// Two events report fullscreen because there are two routes into it. The Fullscreen
// API fires `fullscreenchange`; the browser's own shortcut is visible through the
// display-mode query. Either route is only a chance to ask for the lock — rejection is
// a browser decision and leaves the session running with every key it can receive.

import { BROWSER_RESERVED_CHORD_CODES } from "./macKeys.ts";

export const DEFAULT_LOCK_CODES: readonly string[] = [
  ...BROWSER_RESERVED_CHORD_CODES,
  // Q is intentionally not translated: a Mac remote wants Command-Q. Locking it is
  // what lets that chord reach the remote instead of quitting the local browser.
  "KeyQ",
];

interface KeyboardLockApi {
  lock: (codes?: readonly string[]) => Promise<void>;
  unlock: () => void;
}

function keyboardApi(): KeyboardLockApi | undefined {
  if (typeof navigator === "undefined") {
    return undefined;
  }
  return (navigator as Navigator & { keyboard?: KeyboardLockApi }).keyboard;
}

const FULLSCREEN_QUERY = "(display-mode: fullscreen)";

function fullscreenQuery(): MediaQueryList | undefined {
  if (
    typeof window === "undefined" ||
    typeof window.matchMedia !== "function"
  ) {
    return undefined;
  }
  return window.matchMedia(FULLSCREEN_QUERY);
}

function fullscreenNow(): boolean {
  if (typeof document !== "undefined" && document.fullscreenElement) {
    return true;
  }
  return fullscreenQuery()?.matches === true;
}

let held = false;
let arming: Promise<void> | null = null;

function arm(): Promise<void> {
  if (held) {
    return Promise.resolve();
  }
  if (arming) {
    return arming;
  }
  const keyboard = keyboardApi();
  if (!keyboard) {
    return Promise.resolve();
  }
  const attempt = keyboard
    .lock(DEFAULT_LOCK_CODES)
    .then(
      () => {
        // Fullscreen may have ended while Chromium was deciding. Hand a late grant
        // straight back instead of leaving an untracked lock behind.
        if (fullscreenNow()) {
          held = true;
        } else {
          keyboard.unlock();
        }
      },
      () => {
        // Unsupported or refused is a valid browser outcome. The app/window hosts
        // still deliver every chord, and a tab keeps the keys it normally exposes.
      },
    )
    .finally(() => {
      if (arming === attempt) {
        arming = null;
      }
    });
  arming = attempt;
  return attempt;
}

function disarm(): void {
  if (!held) {
    return;
  }
  keyboardApi()?.unlock();
  held = false;
}

function sync(): void {
  if (fullscreenNow()) {
    void arm();
  } else {
    disarm();
  }
}

if (typeof document !== "undefined") {
  document.addEventListener("fullscreenchange", sync);
  fullscreenQuery()?.addEventListener?.("change", sync);
  sync();
}
