// Keyboard Lock is what immersive full screen is *for*, and it is not a control of its
// own: `fullscreen.ts` is the control, this file is the consequence. Nothing here is
// reported to the user, because a lock is never the difference between a working
// session and a broken one — only between a chord the remote receives and one the host
// acts on first.
//
// Chromium activates a lock in `WebContentsImpl::RequestKeyboardLock` only while
// `IsFullscreenForTabOrPending` holds, which is element full screen and nothing else.
// The browser's own full screen is deliberately not watched here: a
// `(display-mode: fullscreen)` query matches it, and arming on that took a lock
// Chromium never made active — a session that looked full screen and still lost
// Super+E, Alt+Tab and ⌘W to the host. See `fullscreen.ts`.
//
// Every key is locked rather than a list, because that is the whole of the mode: the
// Super key, Alt+Tab and the six chords a browser reserves all belong to the remote
// while it is on, and Escape stops being a key the browser keeps — it leaves the mode
// by being *held* instead. Rejection remains a valid browser outcome and leaves the
// session running with every key it can still receive.

import { isFullscreen, onFullscreenChange } from "./fullscreen.ts";

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
    .lock()
    .then(
      () => {
        // Full screen may have ended while Chromium was deciding. Hand a late grant
        // straight back instead of leaving an untracked lock behind.
        if (isFullscreen()) {
          held = true;
        } else {
          keyboard.unlock();
        }
      },
      () => {
        // Unsupported or refused is a valid browser outcome. An app window still
        // delivers the browser's own chords windowed, and a tab keeps the keys it
        // normally exposes.
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
  if (isFullscreen()) {
    void arm();
  } else {
    disarm();
  }
}

onFullscreenChange(sync);
sync();
