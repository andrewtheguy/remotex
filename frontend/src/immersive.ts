// Fullscreen plus Keyboard Lock: the browser's own way of handing over the chords it
// otherwise keeps for itself.
//
// **Full screen is the trigger, not a button.** A lock is only ever active while the
// window is full screen, so there is nothing for a separate opt-in to decide: any full
// screen this page can observe arms the lock, and leaving disarms it. The alternative
// was a toggle the user had to remember *after* going full screen, whose only possible
// answers were "yes" and a session where ⌘W still closes the tab. `enterImmersive` is
// therefore just a way to *reach* full screen for a menu button to call; the arming
// happens in `sync` no matter who got the window there.
//
// Two events say so, because there are two kinds of full screen and only one of them
// is the Fullscreen API. `requestFullscreen` sets `document.fullscreenElement` and
// fires `fullscreenchange`; the browser's own ⌃⌘F and F11 set neither, and are visible
// only as `(display-mode: fullscreen)` — the same media query `appWindow.ts` reads for
// the opposite purpose. Watching both is what makes the platform's own full screen
// arrive here as immersive rather than as a full screen window that quietly kept ⌘W.
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
// unverified API detail. Here the question does not arise — `lock()` is specified
// against the full screen state, not against a gesture.
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
// this is safe to arm without asking.

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

/// The media query that sees the full screen the Fullscreen API does not own.
///
/// `appWindow.ts` reads the same feature to decide what a window *is*, and treats a
/// `fullscreen` answer as the reason its own answer must be cached. Here it is the
/// live signal rather than the thing to be defended against — read every time, never
/// remembered, because this is precisely the state that moves.
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

/// Whether this window is full screen by either route.
///
/// `document.fullscreenElement` alone would miss ⌃⌘F and F11 entirely; the media query
/// alone would be a slightly later answer than the event that triggers a re-read. The
/// union is what makes "am I full screen" one question with one answer.
function fullscreenNow(): boolean {
  if (typeof document !== "undefined" && document.fullscreenElement) {
    return true;
  }
  return fullscreenQuery()?.matches === true;
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
const lockListeners = new Set<(locked: boolean) => void>();

function setHeld(next: boolean): void {
  if (held === next) {
    return;
  }
  held = next;
  for (const notify of lockListeners) {
    notify(next);
  }
}

// Full screen is tracked as well as the lock, and they are not the same fact. A
// refused `lock()` leaves a window that is full screen and has no chords, and a button
// that reads only the lock would offer to *enter* a mode it is already in — no way
// back out except the platform's own. Two stores, so the way out always tracks the
// window and the promise about ⌘W always tracks the lock.
let active = false;
const activeListeners = new Set<(fullscreen: boolean) => void>();

function setActive(next: boolean): void {
  if (active === next) {
    return;
  }
  active = next;
  for (const notify of activeListeners) {
    notify(next);
  }
}

// In flight, so the two events that report one transition cannot start two locks, and
// so `enterImmersive` can await the attempt its own `requestFullscreen` set going.
let arming: Promise<void> | null = null;

/// Take the lock, if full screen still wants one by the time it is granted.
///
/// The re-read after `lock()` resolves is not defensive tidiness: a held Esc during
/// those milliseconds leaves the browser out of full screen with a lock it granted,
/// and nothing else would ever take it back.
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
        if (fullscreenNow()) {
          setHeld(true);
        } else {
          keyboard.unlock();
        }
      },
      () => {
        // A refused lock is a real outcome and not an error to report: full screen
        // still happened, and `keyboardLockHeld` staying false is how the caller says
        // which of the two the user got.
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
  setHeld(false);
}

/// Bring the lock into line with the window, whichever of the two events reported it.
///
/// Idempotent on purpose: entering by `requestFullscreen` fires `fullscreenchange` and
/// flips the media query, so this runs twice for one transition and must mean the same
/// thing both times.
function sync(): void {
  const fullscreen = fullscreenNow();
  setActive(fullscreen);
  if (fullscreen) {
    void arm();
  } else {
    disarm();
  }
}

// Full screen is the single source of truth in both directions. A held Esc, the ⌃⌘F
// the platform owns, a window manager — none of them call this module, all of them
// produce one of these two signals, and every one of them ends the lock. Watching them
// is what stops the client believing it still has ⌘W after the browser has taken it
// back, and it is now also what gives it ⌘W in the first place.
if (typeof document !== "undefined") {
  document.addEventListener("fullscreenchange", sync);
  fullscreenQuery()?.addEventListener?.("change", sync);
  active = fullscreenNow();
}

/** Whether a lock is held right now. */
export function keyboardLockHeld(): boolean {
  return held;
}

/** Subscribe to lock changes; the returned function detaches the listener. */
export function onKeyboardLockChange(
  handler: (locked: boolean) => void,
): () => void {
  lockListeners.add(handler);
  return () => {
    lockListeners.delete(handler);
  };
}

/**
 * Whether this window is full screen — which is to say, whether it is immersive.
 *
 * The lock follows this, so in every ordinary case the two agree; they part only when
 * a browser refuses the lock, and then this is the one that says how to get out.
 */
export function immersiveActive(): boolean {
  return active;
}

/** Subscribe to full screen changes; the returned function detaches the listener. */
export function onImmersiveChange(
  handler: (fullscreen: boolean) => void,
): () => void {
  activeListeners.add(handler);
  return () => {
    activeListeners.delete(handler);
  };
}

/**
 * Go full screen. **Must be called from a user gesture.**
 *
 * Only a way to reach the state that arms the lock, for a menu button that has the
 * gesture to spend — ⌃⌘F and F11 arrive at the same place without it. Resolves to
 * whether the lock ended up held, which is not the same as whether this worked:
 * full screen without a lock is a real outcome and is not undone.
 */
export async function enterImmersive(): Promise<boolean> {
  try {
    if (!document.fullscreenElement) {
      await document.documentElement.requestFullscreen();
    }
  } catch {
    return false;
  }
  // `sync` has almost certainly started this already; `arm` hands back the same
  // attempt rather than a second one. Calling it here is what makes the result
  // awaitable, and what covers a browser that resolves `requestFullscreen` before it
  // dispatches the event.
  await arm();
  return held;
}

/**
 * Drop the lock and leave full screen.
 *
 * Unlock first, then exit: the other order fires `fullscreenchange` against a lock
 * that is still held, and `sync` would unlock it a second time.
 *
 * Then `sync`, because leaving is a request and not a result. `exitFullscreen` can
 * reject, and a window that got here by ⌃⌘F has no `fullscreenElement` for this to
 * exit at all — in both cases the window is still full screen once the dust settles,
 * no event fires to say otherwise, and the unlock above has taken away chords the
 * window is still entitled to. `sync` gives them back. It is deliberately the same
 * reconciliation the two listeners run rather than a second opinion about the state:
 * anything else is how the two come to disagree.
 *
 * So this is a no-op against the platform's own full screen, which is the truth —
 * only ⌃⌘F or a held Esc leaves that, and no page can.
 */
export async function exitImmersive(): Promise<void> {
  disarm();
  if (document.fullscreenElement) {
    await document.exitFullscreen().catch(() => {});
  }
  sync();
}
