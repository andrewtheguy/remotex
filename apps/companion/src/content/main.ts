// The relay between the page's bus and the extension's.
//
// It exists only on `http://*.remotex.localhost/*`, which the manifest declares as this
// script's `matches` and as the extension's one host permission. So there is no
// whitelist check here: being injected at all *is* the check, and it is Chrome's.
//
// The one gate left is the window kind. **Nothing is posted in an ordinary tab**, and
// that is the whole design rather than a limitation: the extension serves app windows,
// where the page already keeps the browser's chords, and a tab is served by the client
// itself. See docs/companion-extension.md.
//
// This is bundled as a classic script with no imports left in it, because that is what
// Chrome loads a content script as.

import type { CompanionCommand, NativeState } from "../shared/contract.ts";
import { EXT_SOURCE, isPageMessage } from "../shared/contract.ts";
import {
  isToContent,
  type PageReport,
  type ToWorker,
} from "../shared/messages.ts";
import type { ViewportMetrics } from "../shared/resize.ts";

/**
 * The same `display-mode` allow-list `frontend/src/appWindow.ts` uses.
 *
 * Deliberately not `!browser`: a plain tab reports `display-mode: fullscreen` the
 * moment it goes full screen, and fullscreen does not turn it into an app window. Both
 * ends of the bus have to agree on what an app window is, so both ask this way.
 */
const APP_DISPLAY_MODES = [
  "standalone",
  "minimal-ui",
  "window-controls-overlay",
];

function appWindow(): boolean {
  return APP_DISPLAY_MODES.some(
    (mode) => window.matchMedia(`(display-mode: ${mode})`).matches,
  );
}

/**
 * Run `start` as soon as this is an app window, now or later.
 *
 * Later is the case that matters: *Install page as app…* reparents the live document
 * into the new window instead of reloading it, so a script that asked once at
 * `document_start` asked while it was still a tab and would stay silent in the window
 * the user just installed. `frontend/src/appWindow.ts` latches the same way, on the same
 * signal, which is what keeps the two ends of the bus agreeing about the window they
 * are in.
 *
 * One way only. Full screen replaces the display mode with `fullscreen`, so an armed
 * window must not disarm when it enters fullscreen.
 */
function whenAppWindow(start: () => void): void {
  // The latch itself, rather than the listener removal that usually happens to be
  // enough: three queries flip together, and a run that claimed this guard is the
  // only one that may arm the bus.
  let armed = false;
  const announce = () => {
    if (armed) {
      return;
    }
    armed = true;
    start();
  };
  if (appWindow()) {
    announce();
    return;
  }
  const queries = APP_DISPLAY_MODES.map((mode) =>
    window.matchMedia(`(display-mode: ${mode})`),
  );
  const promote = () => {
    if (!appWindow()) {
      return;
    }
    for (const query of queries) {
      query.removeEventListener("change", promote);
    }
    announce();
  };
  for (const query of queries) {
    query.addEventListener("change", promote);
  }
}

/**
 * The page's last state, which is the only thing cached here.
 *
 * A cache of something the page owns, not a second copy of it: the popup asks for a
 * description at the moment it opens, and the page has no way to be asked.
 */
let lastState: NativeState | null = null;

function send(message: ToWorker): void {
  // The worker may be asleep, and waking it is the point; a rejected promise here is a
  // worker that went away mid-message, which the next message re-does anyway.
  void chrome.runtime.sendMessage(message).catch(() => {});
}

function toPage(command: CompanionCommand): void {
  window.postMessage({ source: EXT_SOURCE, ...command }, window.origin);
}

function metrics(): ViewportMetrics {
  // `availLeft` and `availTop` are not in the DOM lib's `Screen` — they predate the
  // standard and every Chromium has them. The work area's origin is the difference
  // between "the second monitor" and "somewhere off the left of the first one".
  const screen = window.screen as Screen & {
    availLeft?: number;
    availTop?: number;
  };
  return {
    innerWidth: window.innerWidth,
    innerHeight: window.innerHeight,
    outerWidth: window.outerWidth,
    outerHeight: window.outerHeight,
    availLeft: screen.availLeft ?? 0,
    availTop: screen.availTop ?? 0,
    availWidth: screen.availWidth,
    availHeight: screen.availHeight,
  };
}

function receiveFromPage(event: MessageEvent): void {
  // The same three guards the client applies to us, in the same order and for the same
  // reasons: an iframe or an opener is not this page, a cross-origin poster is not this
  // page either, and everything else on this bus is somebody else's conversation.
  if (event.source !== window || event.origin !== window.origin) {
    return;
  }
  if (!isPageMessage(event.data)) {
    return;
  }
  const message = event.data;
  if (message.type === "hello") {
    hello();
  } else if (message.type === "state") {
    lastState = message.state;
    send({ to: "worker", type: "state", state: message.state });
  } else if (message.type === "clipboardFromRemote") {
    send({ to: "worker", type: "clipboardFromRemote", text: message.text });
  }
}

function hello(): void {
  toPage({
    type: "hello",
    version: chrome.runtime.getManifest().version,
    // Both are true and neither is a setting: this extension has no options page. They
    // are on the wire because the page must follow what a companion says it does
    // rather than infer it from the fact that one answered.
    capabilities: { clipboard: true, resize: true },
  });
}

whenAppWindow(() => {
  window.addEventListener("message", receiveFromPage);

  chrome.runtime.onMessage.addListener((data, _sender, respond) => {
    if (!isToContent(data)) {
      return false;
    }
    if (data.type === "report") {
      const report: PageReport = { state: lastState, metrics: metrics() };
      respond(report);
      return false;
    }
    if (data.type === "clipboardLocal") {
      toPage({ type: "clipboardLocal", text: data.text });
    }
    return false;
  });

  send({ to: "worker", type: "wake" });
  hello();
  // A bfcache restore replays no injection. The page says hello again on `pageshow`
  // for the mirror-image reason, and either side may be the one that gets there first.
  window.addEventListener("pageshow", hello);
});
