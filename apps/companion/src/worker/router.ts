// What the worker does with each message, as one function over an injected surface.
//
// The surface is injected so `tests/router.test.ts` can drive this with a fake and
// assert the exact calls made. That is most of what is worth testing about a worker:
// the decisions are here and the plumbing is in `main.ts`.
//
// **It holds no state.** An MV3 worker is killed whenever Chrome feels like it, so
// anything remembered between messages is a bug waiting for a quiet afternoon. The
// grants come from `chrome.permissions`, the tab list from `chrome.tabs.query`, and
// the page's state from a `report` round trip made at the moment it is needed.

import type { Rect } from "../shared/geometry.ts";
import type {
  Description,
  PageReport,
  ToContent,
  ToOffscreen,
  ToWorker,
} from "../shared/messages.ts";
import { hostLabelFor, originPatternFor } from "../shared/origin.ts";
import { windowBoundsForRemote } from "../shared/resize.ts";
import { iconStateFor } from "./icon.ts";

/** Everything the router does to the world. Nothing else is reachable from here. */
export interface Surface {
  ensureOffscreen(): Promise<void>;
  toOffscreen(message: ToOffscreen): Promise<void>;
  /** Ask one tab's content script. Null when there is none in it. */
  report(tabId: number): Promise<PageReport | null>;
  /** Tell one tab's content script. Silent when there is none. */
  toContent(tabId: number, message: ToContent): Promise<void>;
  /** Every tab whose origin has been granted, which is where content scripts can be. */
  grantedTabs(): Promise<number[]>;
  tabUrl(tabId: number): Promise<string | undefined>;
  isGranted(pattern: string | null): Promise<boolean>;
  grant(tabId: number): Promise<void>;
  reconcile(): Promise<void>;
  zoom(tabId: number): Promise<number>;
  windowBounds(tabId: number): Promise<Rect | null>;
  setWindowBounds(tabId: number, bounds: Rect): Promise<void>;
  paint(tabId: number): Promise<void>;
  paintAll(): Promise<void>;
}

export async function route(
  message: ToWorker,
  tabId: number | undefined,
  surface: Surface,
): Promise<Description | undefined> {
  switch (message.type) {
    case "wake":
      await surface.ensureOffscreen();
      if (tabId !== undefined) {
        await surface.paint(tabId);
      }
      return;

    case "state": {
      // The enable flag is the one thing pushed unasked, and it is recomputed from the
      // message rather than remembered: polling the system clipboard while there is
      // nothing to send it to is exactly what it exists to prevent.
      const { mode, canClipboard } = message.state;
      await surface.ensureOffscreen();
      await surface.toOffscreen({
        to: "offscreen",
        type: "enable",
        enabled: mode === "desktop" && canClipboard,
      });
      return;
    }

    case "clipboardFromRemote":
      await surface.toOffscreen({
        to: "offscreen",
        type: "fromRemote",
        text: message.text,
      });
      return;

    case "clipboardLocal": {
      // Fanned out rather than addressed, because the worker does not know which window
      // has a live desktop and refuses to remember. A content script with no session
      // hands it to a page whose own guards drop it, and one in a tab does not exist.
      const tabs = await surface.grantedTabs();
      for (const target of tabs) {
        await surface.toContent(target, {
          to: "content",
          type: "clipboardLocal",
          text: message.text,
        });
      }
      return;
    }

    case "describe":
      return describe(message.tabId, surface);

    case "resize":
      await resize(message.tabId, surface);
      return;

    case "granted":
      await surface.grant(message.tabId);
      await surface.paint(message.tabId);
      return;

    case "revoked": {
      // Order matters: the goodbye goes out while the content scripts are still there.
      // Unregistering does not tear down a script already running in an open window,
      // but a page that has navigated since would never hear it.
      const tabs = await surface.grantedTabs();
      for (const target of tabs) {
        await surface.toContent(target, { to: "content", type: "bye" });
      }
      await surface.reconcile();
      await surface.paintAll();
      return;
    }
  }
}

async function describe(tabId: number, surface: Surface): Promise<Description> {
  const url = await surface.tabUrl(tabId);
  const pattern = originPatternFor(url);
  const granted = await surface.isGranted(pattern);
  return {
    pattern,
    host: hostLabelFor(url),
    granted,
    report: granted ? await surface.report(tabId) : null,
  };
}

async function resize(tabId: number, surface: Surface): Promise<void> {
  const report = await surface.report(tabId);
  const remote = report?.state?.size;
  const current = await surface.windowBounds(tabId);
  if (!report || !remote || !current) {
    return;
  }
  const bounds = windowBoundsForRemote({
    remote,
    metrics: report.metrics,
    zoom: await surface.zoom(tabId),
    current,
  });
  if (bounds) {
    await surface.setWindowBounds(tabId, bounds);
  }
}

/** The icon's answer for one tab, which needs the grant and the window kind. */
export async function iconFor(
  tabId: number,
  surface: Surface,
): Promise<ReturnType<typeof iconStateFor>> {
  const pattern = originPatternFor(await surface.tabUrl(tabId));
  const granted = await surface.isGranted(pattern);
  // A granted site with no content script answering is a tab rather than an app window:
  // the script is injected either way, and it only listens in one of them.
  const appWindow = granted ? (await surface.report(tabId)) !== null : false;
  return iconStateFor({ granted, appWindow });
}
