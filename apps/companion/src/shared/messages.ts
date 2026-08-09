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
  /**
   * The page's state, relayed. Nothing is stored for it — it is the sending window's
   * half of the clipboard-polling answer, and the worker asks the others itself.
   */
  | { to: "worker"; type: "state"; state: NativeState }
  /** The remote's clipboard changed; the offscreen document should write it. */
  | { to: "worker"; type: "clipboardFromRemote"; text: string }
  /** The system clipboard changed. From the offscreen document only. */
  | { to: "worker"; type: "clipboardLocal"; text: string }
  /** The popup, opening. Answered with {@link Description}. */
  | { to: "worker"; type: "describe"; tabId: number }
  /** The popup's Resize to display. */
  | { to: "worker"; type: "resize"; tabId: number };

export type ToContent =
  /** Goes to the page as `clipboardLocal`. */
  | { to: "content"; type: "clipboardLocal"; text: string }
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
  /** The host as the user reads it, port and all. Null off an addressable page. */
  host: string | null;
  /** Whether this window is on the one host this extension serves. */
  served: boolean;
  /** Null when there is no content script in this tab, which includes every tab. */
  report: PageReport | null;
}

/**
 * What each recognised type must carry, checked before its union type is claimed.
 *
 * A guard that reads only `to` and `type` is a guard that tells TypeScript a `describe`
 * has a `tabId` when it may not, and the router then hands `undefined` to
 * `chrome.tabs.get`. Nothing hostile can reach this bus — there is no
 * `externally_connectable` — so this is not a defence against an attacker: it is what
 * makes `data is ToWorker` true rather than merely asserted, and it turns a message
 * this build sends wrongly into a dropped one instead of a confusing one.
 */
type Check = (message: Record<string, unknown>) => boolean;

const anything: Check = () => true;
const withText: Check = (message) => typeof message.text === "string";
/** A tab id is an integer. `undefined` and `"3"` both reach Chrome as neither. */
const withTabId: Check = (message) => Number.isInteger(message.tabId);

const WORKER_TYPES: ReadonlyMap<string, Check> = new Map([
  ["wake", anything],
  // Only that it is an object: the fields the router reads are two of a dozen, and a
  // malformed one answers "not a live desktop", which is the safe way to be wrong.
  [
    "state",
    (message) => typeof message.state === "object" && message.state !== null,
  ],
  ["clipboardFromRemote", withText],
  ["clipboardLocal", withText],
  ["describe", withTabId],
  ["resize", withTabId],
]);

const CONTENT_TYPES: ReadonlyMap<string, Check> = new Map([
  ["clipboardLocal", withText],
  ["report", anything],
]);

const OFFSCREEN_TYPES: ReadonlyMap<string, Check> = new Map([
  ["enable", (message) => typeof message.enabled === "boolean"],
  ["fromRemote", withText],
]);

function addressed(
  data: unknown,
  to: string,
  types: ReadonlyMap<string, Check>,
): boolean {
  if (typeof data !== "object" || data === null) {
    return false;
  }
  const message = data as Record<string, unknown>;
  if (message.to !== to || typeof message.type !== "string") {
    return false;
  }
  const check = types.get(message.type);
  if (!check) {
    return false;
  }
  return check(message);
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
