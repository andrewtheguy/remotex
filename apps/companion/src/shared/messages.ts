// The extension's own bus, which is not the page's.
//
// `window.postMessage` carries the page seam and is defined by
// `frontend/src/companion.contract.ts`. This is the other one: `chrome.runtime`
// messages between the four contexts this extension runs in — the service worker, a
// content script, the offscreen document and the popup.
//
// Every message names the context it is **for**, because `chrome.runtime.sendMessage`
// delivers to all of them at once: an offscreen document and a popup both hear what
// the worker hears. Reading a message meant for somebody else is how two contexts end
// up doing the same work, so the `to` field is checked before the `type` is.

import type { NativeState } from "./contract.ts";
import type { ViewportMetrics } from "./resize.ts";

export type ToWorker =
  /** A content script has started. Spins the offscreen document up. */
  | { to: "worker"; type: "wake" }
  /** The page's state, relayed. The worker keeps the last one per tab. */
  | { to: "worker"; type: "state"; state: NativeState }
  /** The remote's clipboard changed; the offscreen document should write it. */
  | { to: "worker"; type: "clipboardFromRemote"; text: string }
  /** The system clipboard changed. From the offscreen document only. */
  | { to: "worker"; type: "clipboardLocal"; text: string }
  /** The popup, opening. Answered with {@link Description}. */
  | { to: "worker"; type: "describe"; tabId: number }
  /** The popup's Resize to display. */
  | { to: "worker"; type: "resize"; tabId: number }
  /**
   * The popup's switch, *after* Chrome has already granted the origin. It carries no
   * origin: `chrome.permissions` holds the answer by the time this is sent, and a copy
   * in the message would be a second version of it that could disagree.
   */
  | { to: "worker"; type: "granted"; tabId: number }
  /** Sent by the worker to itself from `permissions.onRemoved`, for one code path. */
  | { to: "worker"; type: "revoked" };

export type ToContent =
  /** Goes to the page as `clipboardLocal`. */
  | { to: "content"; type: "clipboardLocal"; text: string }
  /** Goes to the page as `bye`: this site's host access was taken away. */
  | { to: "content"; type: "bye" }
  /** Answered with {@link PageReport}. Nothing is asked of the page for it. */
  | { to: "content"; type: "report" };

export type ToOffscreen =
  | { to: "offscreen"; type: "enable"; enabled: boolean }
  | { to: "offscreen"; type: "fromRemote"; text: string };

/** What a content script knows: the page's last state, and its own measurements. */
export interface PageReport {
  state: NativeState | null;
  metrics: ViewportMetrics;
}

/** What the popup draws itself from. */
export interface Description {
  /** The pattern this window would be granted under, or null off an http(s) page. */
  pattern: string | null;
  /** The host as the user reads it, port and all. */
  host: string | null;
  granted: boolean;
  /** Null when there is no content script in this tab, which includes every tab. */
  report: PageReport | null;
}

const WORKER_TYPES: ReadonlySet<string> = new Set([
  "wake",
  "state",
  "clipboardFromRemote",
  "clipboardLocal",
  "describe",
  "resize",
  "granted",
  "revoked",
]);

const CONTENT_TYPES: ReadonlySet<string> = new Set([
  "clipboardLocal",
  "bye",
  "report",
]);

const OFFSCREEN_TYPES: ReadonlySet<string> = new Set(["enable", "fromRemote"]);

function addressed(
  data: unknown,
  to: string,
  types: ReadonlySet<string>,
): boolean {
  if (typeof data !== "object" || data === null) {
    return false;
  }
  const message = data as { to?: unknown; type?: unknown };
  return (
    message.to === to &&
    typeof message.type === "string" &&
    types.has(message.type)
  );
}

export function isToWorker(data: unknown): data is ToWorker {
  return addressed(data, "worker", WORKER_TYPES);
}

export function isToContent(data: unknown): data is ToContent {
  return addressed(data, "content", CONTENT_TYPES);
}

export function isToOffscreen(data: unknown): data is ToOffscreen {
  return addressed(data, "offscreen", OFFSCREEN_TYPES);
}
