// What crosses between this SPA and the RemoteX Companion extension, as types and
// nothing else.
//
// A second seam beside `nativeHost.contract.ts`, and deliberately much narrower.
// `remotex.app` is a *shell*: it owns the window chrome and the menu bar, so the page
// hides its own floating menu and the seam has to carry every control that menu bar
// stands in for. The extension owns none of that — under it `NATIVE_HOST` stays false,
// the floating menu is on screen, and a command here for something that menu already
// does would be a second UI for the same control. So what crosses is only what a page
// in a browser genuinely cannot do for itself:
//
//   page → ext   the remote's clipboard, which a page may not write without a gesture;
//   page → ext   fit this window to the framebuffer, which a page may not resize;
//   ext → page   the system clipboard, which a page may not read while unfocused.
//
// The resize *request* is on the seam even though the arithmetic is not. The page
// reports the framebuffer in `NativeState.size` and the extension measures the window
// from its own content script, so it needs nothing from here to compute the fit — but
// it does need to be asked, and there is nowhere else to ask from. **The extension runs
// only in a Chrome app window**, which has no toolbar and therefore no action icon and
// no popup: the page is the only surface in front of the user. In an ordinary tab
// nothing here happens at all. See docs/companion-extension.md.
//
// Keys are not here at all, and that is the scope rule for the whole extension: it
// does only what the browser cannot. An app window reserves no chords, so a page in
// one keeps ⌘W and Ctrl+W by calling `preventDefault` (`appWindow.ts`); a plain tab
// gets the same from fullscreen plus Keyboard Lock (`immersive.ts`). Both are the
// page's own doing and work with nothing installed.
//
// Apart from `companion.ts` for the same reason `nativeHost.contract.ts` is apart from
// `nativeHost.ts`: **this file must not import React**, or anything that does.
// `apps/companion/src/shared/contract.ts` type-imports it, and that tree has no React
// in its dependencies and no business having one — its `tsc --noEmit` runs in CI on a
// machine where `frontend/node_modules` does not exist.

import type { HostRemoteSize, NativeState } from "./nativeHost.contract.ts";

// Re-exported so the extension has one import to reach for, and so a rename in the
// client is a `tsc` error over there rather than a popup that silently stops
// rendering.
export type { HostRemoteSize, NativeState } from "./nativeHost.contract.ts";

/**
 * The tag on every message, in both directions.
 *
 * `window.postMessage` is a shared bus: the page, any script it loads and every
 * extension's content script all see the same events. Neither side reads anything
 * untagged, and neither side reads its own tag back.
 */
export const PAGE_SOURCE = "remotex-page";
export const EXT_SOURCE = "remotex-ext";

/** What the page tells the extension. */
export type CompanionEvent =
  /**
   * Posted at module load, and again on `pageshow` — a bfcache restore replays no
   * content-script injection, so without it a page that went back and forward would
   * be talking to an extension that had stopped listening.
   *
   * Sending our own hello as well as answering theirs is what makes the handshake
   * order-independent: the content script runs at `document_start` but reads the
   * whitelist asynchronously, so either side can be first and neither may assume it.
   */
  | { type: "hello"; client: string }
  /** The whole of what the extension knows about this session. */
  | { type: "state"; state: NativeState }
  /**
   * The remote's clipboard changed on its own. The extension writes the system
   * clipboard from an offscreen document, which is the one context that can do it
   * without focus and without a gesture.
   */
  | { type: "clipboardFromRemote"; text: string }
  /**
   * Fit this window to the desktop it is showing, at 100%.
   *
   * Carries nothing: the extension has the framebuffer from the last `state` and
   * measures the window itself, and a request that restated either would be a second
   * copy of a number to disagree about. Sent only where a connected companion has
   * said `resize`, so the control that sends it is absent rather than dead.
   */
  | { type: "resizeToDisplay" };

/** What the extension tells the page. */
export type CompanionCommand =
  | { type: "hello"; version: string; capabilities: CompanionCapabilities }
  /**
   * This site left the whitelist, or the extension is going away. The page goes back
   * to reading the clipboard on focus; it does not go back to waiting for a hello.
   */
  | { type: "bye" }
  /** The system clipboard changed, focused or not. Goes to `pushLocalClipboard`. */
  | { type: "clipboardLocal"; text: string };

/**
 * What this companion is actually doing, as opposed to what it could do.
 *
 * Both are user settings on the extension's options page, and the page acts on both.
 * `clipboard` false means the offscreen poller is off and the page must keep reading
 * the clipboard on focus itself. `resize` false means there is no window to fit, so
 * the page offers no control for it — the answer is known before the button is drawn
 * rather than after it is pressed.
 */
export interface CompanionCapabilities {
  clipboard: boolean;
  resize: boolean;
}

/** Handlers for every command the extension can send. */
export type CompanionCommandHandlers = {
  [C in CompanionCommand as C["type"]]: (command: C) => void;
};

/** The envelopes as they appear on the bus. Nothing untagged is ever read. */
export type PageMessage = CompanionEvent & { source: typeof PAGE_SOURCE };
export type ExtMessage = CompanionCommand & { source: typeof EXT_SOURCE };

const EXT_TYPES: ReadonlySet<string> = new Set([
  "hello",
  "bye",
  "clipboardLocal",
]);

const PAGE_TYPES: ReadonlySet<string> = new Set([
  "hello",
  "state",
  "clipboardFromRemote",
  "resizeToDisplay",
]);

function tagged(data: unknown, source: string, types: ReadonlySet<string>) {
  if (typeof data !== "object" || data === null) {
    return false;
  }
  const message = data as { source?: unknown; type?: unknown };
  return (
    message.source === source &&
    typeof message.type === "string" &&
    types.has(message.type)
  );
}

/**
 * Whether an arbitrary `MessageEvent.data` is one of ours, from the extension.
 *
 * This is a security boundary, so it is written to be boring: it never throws, never
 * looks past the two fields it names, and rejects everything it does not recognise.
 * The payload itself is not validated — an extension that ships with this client is
 * not a hostile input, and a `text` of the wrong type is a bug to see, not one to
 * paper over.
 */
export function isExtMessage(data: unknown): data is ExtMessage {
  return tagged(data, EXT_SOURCE, EXT_TYPES);
}

/** The mirror of {@link isExtMessage}, used by the extension's content script. */
export function isPageMessage(data: unknown): data is PageMessage {
  return tagged(data, PAGE_SOURCE, PAGE_TYPES);
}

/** The remote's framebuffer as the popup prints it: `1920 × 1080 @2x`. */
export function describeRemoteSize(size: HostRemoteSize | null): string {
  if (!size) {
    return "—";
  }
  const scale = size.scale > 0 ? size.scale : 1;
  const density = scale === 1 ? "" : ` @${scale}x`;
  return `${Math.round(size.w / scale)} × ${Math.round(size.h / scale)}${density}`;
}
