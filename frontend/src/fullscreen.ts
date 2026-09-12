// Immersive full screen: the page's *own* full screen, which is the only one that hands
// this client the keys it exists to forward.
//
// Chrome has two full screens and they are not interchangeable here:
//
//   - **The browser's own** — the ⛶ beside the zoom row, or F11. It hides the frame and
//     nothing else. `document.fullscreenElement` stays null, Chromium's
//     `IsFullscreenForTabOrPending` stays false, and `RequestKeyboardLock` tests exactly
//     that before it activates a lock. So the host keeps every key it reserves: Super+E
//     opens the local file manager over a remote desktop that fills the screen. A page
//     cannot promote it either — `requestFullscreen` is the only entry Chromium counts,
//     and it is not something the browser's own full screen can be turned into.
//   - **Element full screen**, which this module drives. Chromium activates the pending
//     lock on entry and releases it on exit, so `keyboardLock.ts` holds no control of
//     its own; it follows the same event this does.
//
// Which is why this is a menu item and not something the client arranges for itself:
// `requestFullscreen` needs a user gesture, and the button click is it.
//
// `documentElement`, not the desktop canvas, so the floating menu, its docked panels and
// the Help card are still on screen in the mode. The way out is that button again, or
// holding Escape, which is Chromium's own exit from a locked full screen.

/** Whether this browser will grant element full screen at all. */
export function fullscreenSupported(): boolean {
  return typeof document !== "undefined" && document.fullscreenEnabled;
}

/** Whether the page is in element full screen — never true for the browser's own. */
export function isFullscreen(): boolean {
  return typeof document !== "undefined" && document.fullscreenElement !== null;
}

/** Be told when {@link isFullscreen} changes; detach with the returned function. */
export function onFullscreenChange(handler: () => void): () => void {
  if (typeof document === "undefined") {
    return () => {};
  }
  document.addEventListener("fullscreenchange", handler);
  return () => {
    document.removeEventListener("fullscreenchange", handler);
  };
}

/**
 * Enter or leave, resolving once the browser has answered.
 *
 * Rejection is handed back rather than swallowed: a request refused for want of a user
 * gesture, or by a permissions policy, changes no state at all, and a button that
 * silently does nothing is the one outcome a full-screen control must not have.
 */
export function toggleFullscreen(): Promise<void> {
  if (typeof document === "undefined") {
    return Promise.resolve();
  }
  if (isFullscreen()) {
    return document.exitFullscreen();
  }
  return document.documentElement.requestFullscreen({ navigationUI: "hide" });
}
