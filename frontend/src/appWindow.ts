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
// One place needs more than that, and it is a Mac: `macKeys.ts` decides by table which
// Command chords become Control chords, and the six a browser normally keeps are absent
// from it precisely because they never arrive. In an app window they do.
// `tmp/programs_for_reference/chrome_extension_spike/PWA_KEYS.md` is the per-key
// measurement that goes with this, including the ones no window of any kind is given:
// ⌘Tab, ⌘Space, Alt+F4, the Windows key.
//
// The other reader is `companion.ts`. The companion extension runs *only* in an app
// window, so this is what turns the seam on at all — see docs/companion-extension.md.
//
// Read as the three *app* display modes rather than as "not `browser`", because a plain
// tab reports `display-mode: fullscreen` the moment it goes full screen, and that window
// is given no chords at all: it is `immersive.ts`'s keyboard lock that changes a tab's
// answer, not the fullscreen underneath it. `immersive.ts` watches that same `fullscreen`
// answer live, for the opposite reason — it is the only way to see a ⌃⌘F — which is why
// the two modules read one media feature and disagree about whether it may move.
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

let cached: boolean | null = null;

/**
 * Whether this page is an app window. Answered once, then remembered.
 *
 * Remembered because the answer must not move: full screen replaces the display mode
 * with `fullscreen`, so an app window that entered immersive would stop looking like
 * one and take the Command chord table down with it. The first call is at module load,
 * where nothing is full screen yet — a window cannot start that way, it takes a
 * gesture.
 */
export function appWindow(): boolean {
  if (cached === null) {
    cached =
      typeof window !== "undefined" &&
      typeof window.matchMedia === "function" &&
      isAppWindow((query) => window.matchMedia(query));
  }
  return cached;
}
