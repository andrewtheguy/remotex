// Whether this page is its own window rather than a tab in one.
//
// Chrome calls it an app window: *Install page as app…*, or `chrome --app=<url>`.
// It has no tab strip and no omnibox, which is the visible half. The half that
// matters here is one line in Chromium's `browser_command_controller.cc`, ahead of
// every case in `IsReservedCommandOrKey`:
//
//     // In Apps mode, no keys are reserved.
//
// In a tab, Chrome acts on ⌘W/Ctrl+W, ⌘T and ⌘N itself, before the renderer, and the
// page never sees the keydown. In an app window every one of them is delivered to the
// page first, and `preventDefault()` is the whole of what it takes to keep it — which
// this client already calls on every key the desktop surface sees. So on Windows and
// Linux a shim window captures the browser's chords with no fullscreen, no Keyboard
// Lock and no code at all.
//
// On a Mac, `macKeys.ts` maps the Command chords that arrive to their remote Control
// equivalents. Its table is fixed; this window kind decides whether Chrome delivers
// the six shortcuts a normal tab keeps for itself.
// `tmp/programs_for_reference/chrome_extension_spike/PWA_KEYS.md` is the per-key
// measurement that goes with this, including the ones no window of any kind is given:
// ⌘Tab, ⌘Space, Alt+F4, the Windows key.
//
// The other reader is `companion.ts`. The companion extension runs *only* in an app
// window, so this is what turns the seam on at all — see docs/companion-extension.md.
//
// Read as the three *app* display modes rather than as "not `browser`", because a plain
// tab reports `display-mode: fullscreen` the moment it goes full screen. Fullscreen
// does not turn a tab into the app window the companion extension is allowed to serve.
const APP_DISPLAY_MODES = [
  "standalone",
  "minimal-ui",
  "window-controls-overlay",
];

/**
 * The decision, over a `matchMedia`-shaped function.
 *
 * Taken apart from {@link appWindow} so it can be tested without a window, and so the
 * allow-list above is checked rather than the code around it.
 */
export function isAppWindow(
  match: (query: string) => { matches: boolean },
): boolean {
  return APP_DISPLAY_MODES.some(
    (mode) => match(`(display-mode: ${mode})`).matches,
  );
}

/** As much of a `MediaQueryList` as this module has any use for. */
interface QueryLike {
  matches: boolean;
  addEventListener?: (type: "change", handler: () => void) => void;
  removeEventListener?: (type: "change", handler: () => void) => void;
}

/** The answer, and a way to be told when it arrives. */
export interface AppWindowStore {
  get(): boolean;
  subscribe(handler: () => void): () => void;
}

/**
 * The answer over time, which **latches true and never goes back**.
 *
 * Both halves are load-bearing, and each fixes what the other would get wrong:
 *
 * - *Never goes back*, because full screen replaces the display mode with `fullscreen`.
 *   An app window would otherwise stop looking like one and take the companion seam
 *   down with it mid-session.
 * - *Latches*, rather than being answered once at module load, because **a tab can
 *   become an app window under a running document**. *Install page as app…* reparents
 *   the live tab into the new window instead of reloading it (Chromium's
 *   `ReparentWebContentsIntoAppBrowser`), so a page that answered at load answered as a
 *   tab and went on saying so until somebody closed the window and launched the app
 *   again. Which is exactly what it looked like: an app window insisting it was a tab.
 *
 * A false answer is therefore never final. It is re-read on every `get`, and the media
 * queries turn the change into a notification so nothing has to poll for it.
 *
 * Injectable for the same reason {@link isAppWindow} is: the latch is the interesting
 * part and it is testable without a browser.
 */
export function createAppWindowStore(
  match: (query: string) => QueryLike,
): AppWindowStore {
  const listeners = new Set<() => void>();
  let latched = isAppWindow(match);

  const get = (): boolean => {
    if (!latched && isAppWindow(match)) {
      latched = true;
    }
    return latched;
  };

  if (!latched) {
    // Held in a variable, not matched and dropped: a `MediaQueryList` nothing references
    // may be collected, and its listener with it. All three modes are watched rather
    // than `standalone` alone, for the same reason the allow-list exists — which one an
    // installed window reports is Chrome's choice, not ours.
    const queries = APP_DISPLAY_MODES.map((mode) =>
      match(`(display-mode: ${mode})`),
    );
    // Three queries change together for one transition and a browser may dispatch all
    // three before this detaches any of them, so "told once" is this flag's doing
    // rather than the `removeEventListener` below.
    //
    // The flag is deliberately *not* the latch. A `get` between the display mode
    // changing and the handlers running — a React render is enough — latches on its own
    // re-read, and a promote that took `latched` for "already announced" would then
    // return having told nobody, leaving every subscriber on the wrong answer for the
    // life of the window. Which is the bug this whole file exists to fix, arrived at
    // from the other end.
    let announced = false;
    const promote = () => {
      // `get` does the re-read and the latch; this only has to notice it has happened.
      // A change *away* from an app mode is what the latch swallows, which is why this
      // cannot simply forward the query's own `matches`.
      if (announced || !get()) {
        return;
      }
      announced = true;
      for (const query of queries) {
        query.removeEventListener?.("change", promote);
      }
      for (const notify of [...listeners]) {
        notify();
      }
    };
    for (const query of queries) {
      query.addEventListener?.("change", promote);
    }
  }

  return {
    get,
    subscribe(handler: () => void): () => void {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}

/** Nothing to read and nothing to wait for: server rendering, and a browser with no
 * `matchMedia` at all, which is not a browser this client starts in. */
const NO_WINDOW: AppWindowStore = {
  get: () => false,
  subscribe: () => () => {},
};

/**
 * The one store this page has, built on first use rather than at import.
 *
 * Lazily, because a module's import order is not the client's to arrange: this file is
 * pulled in by the chord table, the floating menu and the companion seam, and building
 * a store at import would bind it to whichever `window` existed at that moment. In a
 * browser there is exactly one and it is already there; under a test runner sharing one
 * module registry, first *use* is the honest moment.
 */
let store: AppWindowStore | null = null;

function shared(): AppWindowStore {
  if (!store) {
    store =
      typeof window !== "undefined" && typeof window.matchMedia === "function"
        ? createAppWindowStore((query) => window.matchMedia(query))
        : NO_WINDOW;
  }
  return store;
}

/** Whether this page is an app window. See {@link createAppWindowStore}. */
export function appWindow(): boolean {
  return shared().get();
}

/**
 * Subscribe to this window *becoming* an app window; detach with the returned function.
 *
 * One-way, and it fires at most once, because that is the shape of the fact: the answer
 * only ever goes false → true, so a subscriber never has to handle losing it.
 */
export function onAppWindowChange(handler: () => void): () => void {
  return shared().subscribe(handler);
}
