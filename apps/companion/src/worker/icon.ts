// The toolbar icon, which says whether this extension is doing anything here.
//
// **Cosmetic and best-effort.** The gate is the content script's, and it is always
// right; per-tab icon state is reset by Chrome on navigation and the worker may be
// asleep when a tab is created, so a stale icon is a thing that happens. Nobody should
// ever make this authoritative.

/** The three answers, which are three different things to tell somebody. */
export type IconState = "on" | "not-granted" | "not-app-window";

const TITLES: Record<IconState, string> = {
  on: "RemoteX Companion is active here",
  "not-granted": "RemoteX Companion — click to enable it for this site",
  "not-app-window":
    "RemoteX Companion works in an app window, not a tab — Chrome menu → Install page as app",
};

/**
 * What to show, given what is known about a tab.
 *
 * Pure, so the decision is testable without a browser. The window kind is the content
 * script's answer, relayed: a worker cannot ask a `display-mode` media query itself,
 * and a tab with no content script in it has not answered — which reads as
 * `not-granted`, since a granted site would have one.
 */
export function iconStateFor(input: {
  granted: boolean;
  appWindow: boolean;
}): IconState {
  if (!input.granted) {
    return "not-granted";
  }
  return input.appWindow ? "on" : "not-app-window";
}

export async function paintTab(tabId: number, state: IconState): Promise<void> {
  const variant = state === "on" ? "on" : "off";
  // `path`, never `imageData`: a service worker has no document to draw one with.
  await chrome.action.setIcon({
    tabId,
    path: {
      16: `icons/${variant}-16.png`,
      32: `icons/${variant}-32.png`,
      48: `icons/${variant}-48.png`,
      128: `icons/${variant}-128.png`,
    },
  });
  await chrome.action.setTitle({ tabId, title: TITLES[state] });
}
