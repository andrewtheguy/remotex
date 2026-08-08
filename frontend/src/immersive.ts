// Fullscreen plus Keyboard Lock: the browser's own way of handing over the chords it
// otherwise keeps for itself.
//
// **No extension is involved, and that is the point.** `requestFullscreen` and
// `navigator.keyboard` are plain web APIs on any secure-context page in Chromium, so
// this works in stock Chrome, Edge and ChromeOS with nothing installed. The companion
// extension exists for the two things a page genuinely cannot do — the system
// clipboard without focus, and its own window's size — and reimplementing this inside
// it would be a second implementation of something the page already has.
//
// It is also why the lock lives here rather than in a content script, which is where
// the spike put it: a content script's isolated world inherits the page's transient
// activation, but only probably, and the spike's README flagged that as its one
// unverified API detail. In the page's own click handler there is no question left.
//
// What a lock buys is exactly `BROWSER_RESERVED_CHORD_CODES` — the six Command chords
// `macKeys.ts` will not translate otherwise — plus Q, which is what stops ⌘Q quitting
// the browser out from under a live session. Everything else already reaches a page.
//
// Keyboard Lock needs a secure context, and nothing here checks for one: this client
// refuses to start outside a secure context, so there is no insecure case to have a
// second path for.
//
// What it cannot buy, on any platform and with any list: the compositor's own keys.
// ⌘Tab, ⌘Space, ⌘⇧3/4/5 and Mission Control never arrive, and a held Esc always exits
// the lock. That escape hatch is deliberate and uncapturable, and it is the reason
// this is safe to offer at all.

import { BROWSER_RESERVED_CHORD_CODES } from "./macKeys.ts";

/**
 * The codes handed to `navigator.keyboard.lock`.
 *
 * The six from the chord table, so the two lists cannot drift, plus `KeyQ`. Q is not
 * in the table on purpose — it is forwarded as an unmapped Meta chord, which is
 * harmless to a Windows guest and correct for a Mac one — but it must be *locked*, or
 * ⌘Q ends the browser.
 *
 * Nothing else is locked. A lock is a promise to the user that only the keys they
 * were told about stop behaving normally, and a wide one is how ⌘C stops copying.
 */
export const DEFAULT_LOCK_CODES: readonly string[] = [
  ...BROWSER_RESERVED_CHORD_CODES,
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

/**
 * Whether this browser can do it at all.
 *
 * Feature detection only. Keyboard Lock also requires a secure context, and there is
 * no check for that here or anywhere else in this client: a non-secure context is
 * refused at startup, so by the time anything can call this the page is on one.
 * Firefox and Safari have no `navigator.keyboard`, which is what this actually asks.
 */
export function available(): boolean {
  if (typeof document === "undefined") {
    return false;
  }
  const keyboard = keyboardApi();
  return (
    typeof keyboard?.lock === "function" &&
    typeof document.documentElement.requestFullscreen === "function"
  );
}

let held = false;
const listeners = new Set<(locked: boolean) => void>();

function setHeld(next: boolean): void {
  if (held === next) {
    return;
  }
  held = next;
  for (const notify of listeners) {
    notify(next);
  }
}

// Fullscreen is the single source of truth for *leaving*. A held Esc, the ⌃⌘F the
// platform owns, a window manager — all of them end fullscreen without telling this
// module anything, and every one of them ends the lock too. Watching the one event
// they all produce is what stops the client believing it still has ⌘W after the
// browser has taken it back.
if (typeof document !== "undefined") {
  document.addEventListener("fullscreenchange", () => {
    if (!document.fullscreenElement && held) {
      keyboardApi()?.unlock();
      setHeld(false);
    }
  });
}

/** Whether a lock is held right now. */
export function keyboardLockHeld(): boolean {
  return held;
}

/** Subscribe to lock changes; the returned function detaches the listener. */
export function onKeyboardLockChange(
  handler: (locked: boolean) => void,
): () => void {
  listeners.add(handler);
  return () => {
    listeners.delete(handler);
  };
}

/**
 * Go fullscreen and take the lock. **Must be called from a user gesture.**
 *
 * Resolves to whether the lock was taken. Fullscreen without a lock is still a real
 * outcome, so a rejected `lock()` leaves the page fullscreen rather than undoing it;
 * the caller shows the difference.
 */
export async function enterImmersive(
  codes: readonly string[] = DEFAULT_LOCK_CODES,
): Promise<boolean> {
  const keyboard = keyboardApi();
  try {
    if (!document.fullscreenElement) {
      await document.documentElement.requestFullscreen();
    }
  } catch {
    return false;
  }
  if (!keyboard) {
    return false;
  }
  try {
    await keyboard.lock(codes);
  } catch {
    return false;
  }
  setHeld(true);
  return true;
}

/**
 * Drop the lock and leave fullscreen.
 *
 * Unlock first, then exit: the other order fires `fullscreenchange` against a lock
 * that is still held, and the handler above would unlock it a second time.
 */
export async function exitImmersive(): Promise<void> {
  keyboardApi()?.unlock();
  setHeld(false);
  if (document.fullscreenElement) {
    await document.exitFullscreen().catch(() => {});
  }
}
