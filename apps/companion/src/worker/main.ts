// The service worker: listeners at the top level, and a real `chrome` behind the
// router's surface.
//
// Every listener is registered synchronously as this module evaluates, because that is
// the one thing MV3 asks of a worker — it is killed and restarted constantly, and an
// event it was not listening for when it woke is an event it never hears.

import type { Rect } from "../shared/geometry.ts";
import type {
  Description,
  PageReport,
  ToContent,
  ToOffscreen,
} from "../shared/messages.ts";
import { isToWorker } from "../shared/messages.ts";
import { isCompanionUrl } from "../shared/origin.ts";
import { paintTab } from "./icon.ts";
import { ensureOffscreen } from "./offscreen.ts";
import { iconFor, route, type Surface } from "./router.ts";

const surface: Surface = {
  ensureOffscreen,

  async toOffscreen(message: ToOffscreen) {
    await ensureOffscreen();
    await quiet(chrome.runtime.sendMessage(message));
  },

  async report(tabId: number): Promise<PageReport | null> {
    // A tab with no content script rejects rather than answers, and so does one whose
    // script is there but in an ordinary tab, where nothing is listening. Both mean
    // the same thing to every caller: no page to ask.
    try {
      const answer = await chrome.tabs.sendMessage(tabId, {
        to: "content",
        type: "report",
      });
      return (answer as PageReport | undefined) ?? null;
    } catch {
      return null;
    }
  },

  async toContent(tabId: number, message: ToContent) {
    await quiet(chrome.tabs.sendMessage(tabId, message));
  },

  // A tab this extension has no host permission for reports no `url` at all, so the
  // filter is doubly true: Chrome hides every window that is not the served host, and
  // the predicate agrees with the manifest about the ones it does show.
  async servedTabs(): Promise<number[]> {
    const tabs = await chrome.tabs.query({});
    return tabs
      .filter((tab) => isCompanionUrl(tab.url))
      .map((tab) => tab.id)
      .filter((id): id is number => id !== undefined);
  },

  async tabUrl(tabId: number) {
    try {
      return (await chrome.tabs.get(tabId)).url;
    } catch {
      return undefined;
    }
  },

  async zoom(tabId: number) {
    try {
      return await chrome.tabs.getZoom(tabId);
    } catch {
      return 1;
    }
  },

  async windowBounds(tabId: number): Promise<Rect | null> {
    const window = await windowOf(tabId);
    if (
      !window ||
      window.left === undefined ||
      window.top === undefined ||
      window.width === undefined ||
      window.height === undefined
    ) {
      return null;
    }
    return {
      x: window.left,
      y: window.top,
      width: window.width,
      height: window.height,
    };
  },

  async setWindowBounds(tabId: number, bounds: Rect) {
    const window = await windowOf(tabId);
    if (window?.id === undefined) {
      return;
    }
    // `state: "normal"` first, in the same call: a maximized window ignores a size.
    await quiet(
      chrome.windows.update(window.id, {
        state: "normal",
        left: bounds.x,
        top: bounds.y,
        width: bounds.width,
        height: bounds.height,
      }),
    );
  },

  async paint(tabId: number) {
    await quiet(paintTab(tabId, await iconFor(tabId, surface)));
  },

  async paintAll() {
    const tabs = await chrome.tabs.query({});
    for (const tab of tabs) {
      if (tab.id !== undefined) {
        await surface.paint(tab.id);
      }
    }
  },
};

async function windowOf(tabId: number): Promise<chrome.windows.Window | null> {
  try {
    const tab = await chrome.tabs.get(tabId);
    return tab.windowId === undefined
      ? null
      : await chrome.windows.get(tab.windowId);
  } catch {
    return null;
  }
}

/** A message to a context that has gone is not an error worth having. */
async function quiet(work: Promise<unknown>): Promise<void> {
  try {
    await work;
  } catch {
    // Deliberately nothing.
  }
}

chrome.runtime.onMessage.addListener((data, sender, respond) => {
  if (!isToWorker(data)) {
    return false;
  }
  route(data, sender.tab?.id, surface)
    .then((description: Description | undefined) => respond(description))
    .catch((error: unknown) => {
      console.error("companion: routing failed", error);
      respond(undefined);
    });
  // True, and it has to be: the answer goes out after an await, and returning false
  // here would close the channel before `describe` had one to send.
  return true;
});

// Nothing to reconcile: the content script is declared in the manifest, so Chrome
// registers it and keeps it registered. Both events exist only to repaint an icon that
// Chrome has already reset, or has never been painted at all.
chrome.runtime.onInstalled.addListener(() => {
  void surface.paintAll();
});

chrome.runtime.onStartup.addListener(() => {
  void surface.paintAll();
});

// `status === "loading"` covers a navigation; `changeInfo.url` on its own is how a
// SPA's `pushState` shows up, and this client is a SPA. Per-tab icon state is reset by
// Chrome on navigation, so this is required rather than an optimisation.
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === "loading" || changeInfo.url !== undefined) {
    void surface.paint(tabId);
  }
});

chrome.tabs.onActivated.addListener(({ tabId }) => {
  void surface.paint(tabId);
});

chrome.windows.onFocusChanged.addListener((windowId) => {
  if (windowId === chrome.windows.WINDOW_ID_NONE) {
    return;
  }
  void chrome.tabs
    .query({ active: true, windowId })
    .then((tabs) => {
      const tabId = tabs[0]?.id;
      return tabId === undefined ? undefined : surface.paint(tabId);
    })
    .catch(() => {});
});
