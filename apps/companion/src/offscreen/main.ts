// The clipboard poller, in the one context allowed to be it.
//
// The synchronizer itself is `apps/viewer/src/main/clipboard.ts`, shared rather than
// reimplemented: it already has the size cap the gateway enforces, the poll interval,
// and three echo guards — including the one the reference spike lacked, where a newer
// local value wins so the remote's clipboard cannot stomp something copied here a
// moment ago. Only the pasteboard underneath it is different, and that is injected.

import {
  ClipboardSynchronizer,
  POLL_INTERVAL_MS,
} from "../../../viewer/src/main/clipboard.ts";
import { isToOffscreen, type ToWorker } from "../shared/messages.ts";
import { pasteboard } from "./pasteboard.ts";

const clipboard = new ClipboardSynchronizer({
  pasteboard,
  send(text: string) {
    const message: ToWorker = { to: "worker", type: "clipboardLocal", text };
    void chrome.runtime.sendMessage(message).catch(() => {});
  },
  schedule(tick: () => void) {
    const timer = setInterval(tick, POLL_INTERVAL_MS);
    return () => clearInterval(timer);
  },
});

chrome.runtime.onMessage.addListener((data) => {
  if (!isToOffscreen(data)) {
    return false;
  }
  if (data.type === "enable") {
    clipboard.update(data.enabled);
  } else {
    clipboard.receiveFromRemote(data.text);
  }
  return false;
});
