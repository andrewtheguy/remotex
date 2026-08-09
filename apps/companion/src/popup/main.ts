// The popup: a state card and one button.
//
// There is nothing to turn on here. The served host is in the manifest, so a window is
// either on it or it is not, and the popup's whole job is to say which and to offer the
// one action that needs a click — Resize to display, which moves the window this popup
// is hanging off.

import { describeRemoteSize } from "../shared/contract.ts";
import type { Description, ToWorker } from "../shared/messages.ts";
import { COMPANION_MATCH } from "../shared/origin.ts";

const root = document.getElementById("popup") as HTMLElement;

async function ask(message: ToWorker): Promise<Description | undefined> {
  return (await chrome.runtime.sendMessage(message)) as Description | undefined;
}

async function currentTab(): Promise<chrome.tabs.Tab | undefined> {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab;
}

function element<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) {
    node.className = className;
  }
  if (text !== undefined) {
    node.textContent = text;
  }
  return node;
}

function describeRow(term: string, value: string): [HTMLElement, HTMLElement] {
  return [element("dt", undefined, term), element("dd", undefined, value)];
}

function render(tabId: number, description: Description): void {
  root.replaceChildren();
  root.removeAttribute("aria-busy");

  if (!description.served) {
    root.append(
      element(
        "p",
        "muted",
        // The one thing worth saying about a window this extension does nothing in: it
        // is the address that decides, and the address is not a setting anywhere. Said
        // with the redirect's own limit in it, because a gateway reached over a LAN
        // address or a proxy is never sent anywhere and would otherwise be left waiting
        // for a redirect that is not coming.
        `The companion serves ${COMPANION_MATCH}, and nothing else — open the gateway at http://<label>.remotex.localhost:<port>/. Its [server].dev_subdomain key sends you there from a loopback address; any other address is left alone.`,
      ),
    );
    return;
  }

  root.append(element("h1", undefined, description.host ?? "—"));

  if (!description.report) {
    root.append(
      element(
        "p",
        "muted",
        "This is an ordinary tab. The companion runs in an app window — Chrome menu → Install page as app.",
      ),
    );
    return;
  }

  const state = description.report.state;
  const card = element("dl");
  card.append(
    ...describeRow("Gateway", state?.branding ?? "—"),
    ...describeRow(
      "Screen",
      state ? (state.mode === "desktop" ? state.status : "Target picker") : "—",
    ),
    ...describeRow("Remote", describeRemoteSize(state?.size ?? null)),
    ...describeRow("Clipboard", state?.canClipboard ? "Syncing" : "Off"),
  );
  root.append(card);

  const resize = element("button", undefined, "Resize to display");
  resize.disabled = !state?.size;
  resize.addEventListener("click", () => {
    void chrome.runtime
      .sendMessage({ to: "worker", type: "resize", tabId } satisfies ToWorker)
      .then(() => window.close())
      .catch(() => {});
  });
  root.append(resize);
}

const tab = await currentTab();
if (tab?.id === undefined) {
  root.replaceChildren(element("p", "muted", "No window to look at."));
  root.removeAttribute("aria-busy");
} else {
  const description = await ask({
    to: "worker",
    type: "describe",
    tabId: tab.id,
  });
  if (description) {
    render(tab.id, description);
  } else {
    root.replaceChildren(
      element(
        "p",
        "muted",
        "The companion's background worker did not answer.",
      ),
    );
    root.removeAttribute("aria-busy");
  }
}
