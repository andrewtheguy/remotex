// The popup: a switch, a state card and one button.
//
// It is the only place a site is ever turned on, and that is not a UI decision — it is
// where the user gesture is. `chrome.permissions.request` needs one, and a popup click
// is the only gesture this extension ever receives.

import { describeRemoteSize } from "../shared/contract.ts";
import type { Description, ToWorker } from "../shared/messages.ts";

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

  if (!description.pattern || !description.host) {
    root.append(
      element(
        "p",
        "muted",
        "This is not a page the companion can be enabled for.",
      ),
    );
    return;
  }

  const row = element("div", "row");
  row.append(element("h1", undefined, description.host));
  const toggle = element(
    "button",
    undefined,
    description.granted ? "Turn off" : "Turn on",
  );
  toggle.addEventListener("click", () => {
    void switchSite(tabId, description);
  });
  row.append(toggle);
  root.append(row);

  if (!description.granted) {
    root.append(
      element(
        "p",
        "muted",
        // The widening is said here rather than buried in a doc, because this is the
        // moment it happens and Chrome's own prompt will show the pattern without one.
        description.pattern.includes(description.host)
          ? "Chrome will ask for permission to read and change this site."
          : `Chrome will ask for ${description.pattern} — a permission cannot name a port, so this covers every port on this host.`,
      ),
    );
    return;
  }

  if (!description.report) {
    root.append(
      element(
        "p",
        "muted",
        "Enabled, but this is an ordinary tab. The companion runs in an app window — Chrome menu → Install page as app.",
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

async function switchSite(
  tabId: number,
  description: Description,
): Promise<void> {
  const origins = [description.pattern as string];
  if (description.granted) {
    // The worker hears `permissions.onRemoved` and says goodbye to the pages; nothing
    // is sent from here for it.
    await chrome.permissions.remove({ origins });
  } else {
    const allowed = await chrome.permissions.request({ origins });
    if (!allowed) {
      return;
    }
    await chrome.runtime.sendMessage({
      to: "worker",
      type: "granted",
      tabId,
    } satisfies ToWorker);
  }
  window.close();
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
