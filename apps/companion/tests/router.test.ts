// The worker's decisions, without a browser.
//
// `route` takes its whole world as an injected surface, so this drives it with a fake
// and asserts the calls made. That is most of what is worth testing about a worker: the
// plumbing is `worker/main.ts` and the decisions are all here.

import assert from "node:assert/strict";
import { test } from "node:test";
import type { NativeState } from "../src/shared/contract.ts";
import type { Rect } from "../src/shared/geometry.ts";
import type { PageReport } from "../src/shared/messages.ts";
import { iconFor, route, type Surface } from "../src/worker/router.ts";

interface Calls {
  offscreen: unknown[];
  content: { tabId: number; message: unknown }[];
  bounds: { tabId: number; bounds: Rect }[];
  painted: number[];
  paintedAll: number;
  granted: number[];
  reconciled: number;
  ensured: number;
}

function fake(over: Partial<Surface> = {}): { surface: Surface; calls: Calls } {
  const calls: Calls = {
    offscreen: [],
    content: [],
    bounds: [],
    painted: [],
    paintedAll: 0,
    granted: [],
    reconciled: 0,
    ensured: 0,
  };
  const surface: Surface = {
    async ensureOffscreen() {
      calls.ensured += 1;
    },
    async toOffscreen(message) {
      calls.offscreen.push(message);
    },
    async report() {
      return null;
    },
    async toContent(tabId, message) {
      calls.content.push({ tabId, message });
    },
    async grantedTabs() {
      return [];
    },
    async tabUrl() {
      return undefined;
    },
    async isGranted() {
      return false;
    },
    async grant(tabId) {
      calls.granted.push(tabId);
    },
    async reconcile() {
      calls.reconciled += 1;
    },
    async zoom() {
      return 1;
    },
    async windowBounds() {
      return null;
    },
    async setWindowBounds(tabId, bounds) {
      calls.bounds.push({ tabId, bounds });
    },
    async paint(tabId) {
      calls.painted.push(tabId);
    },
    async paintAll() {
      calls.paintedAll += 1;
    },
    ...over,
  };
  return { surface, calls };
}

/** A window with 16 DIPs of chrome across and 100 down. See resize.test.ts. */
const METRICS = {
  innerWidth: 800,
  innerHeight: 600,
  outerWidth: 816,
  outerHeight: 700,
  availLeft: 0,
  availTop: 0,
  availWidth: 3000,
  availHeight: 2000,
};

function state(over: Partial<NativeState> = {}): NativeState {
  return {
    branding: "remotex",
    mode: "desktop",
    status: "connected",
    ready: true,
    editing: false,
    size: { w: 1920, h: 1080, scale: 1 },
    canClipboard: true,
    canAudio: false,
    audioEnabled: false,
    audioError: null,
    displays: [],
    activeDisplayId: null,
    macKeyOverridesEnabled: false,
    macKeyOverridesActive: false,
    remoteIsMac: false,
    ...over,
  };
}

test("a live desktop with a clipboard turns the poller on", () => {
  const { surface, calls } = fake();
  return route(
    { to: "worker", type: "state", state: state() },
    1,
    surface,
  ).then(() => {
    assert.deepEqual(calls.offscreen, [
      { to: "offscreen", type: "enable", enabled: true },
    ]);
  });
});

test("the picker turns it off, and so does a target without one", async () => {
  for (const off of [{ mode: "picker" as const }, { canClipboard: false }]) {
    const { surface, calls } = fake();
    await route({ to: "worker", type: "state", state: state(off) }, 1, surface);
    // Not tidiness: polling the system clipboard while there is nothing to send it to
    // is the whole thing the flag exists to prevent.
    assert.deepEqual(calls.offscreen, [
      { to: "offscreen", type: "enable", enabled: false },
    ]);
  }
});

test("another window's live desktop keeps the poller on", async () => {
  // There is one offscreen document, so its flag is one answer for the whole browser.
  // A second gateway reaching its target picker must not turn the clipboard off
  // underneath the first one's desktop — which it would, silently, if the flag were
  // read out of whichever `state` arrived last.
  const asked: number[] = [];
  const { surface, calls } = fake({
    async grantedTabs() {
      return [1, 2];
    },
    async report(tabId) {
      asked.push(tabId);
      return tabId === 2 ? { state: state(), metrics: METRICS } : null;
    },
  });

  await route(
    { to: "worker", type: "state", state: state({ mode: "picker" }) },
    1,
    surface,
  );

  assert.deepEqual(calls.offscreen, [
    { to: "offscreen", type: "enable", enabled: true },
  ]);
  // The sender is not asked back: its answer arrived in the message.
  assert.deepEqual(asked, [2]);
});

test("with every window on the picker it goes off after all", async () => {
  const { surface, calls } = fake({
    async grantedTabs() {
      return [1, 2];
    },
    async report() {
      return { state: state({ mode: "picker" }), metrics: METRICS };
    },
  });
  await route(
    { to: "worker", type: "state", state: state({ mode: "picker" }) },
    1,
    surface,
  );
  assert.deepEqual(calls.offscreen, [
    { to: "offscreen", type: "enable", enabled: false },
  ]);
});

test("a live sender needs nobody else asked", async () => {
  let asked = 0;
  const { surface, calls } = fake({
    async grantedTabs() {
      return [1, 2, 3];
    },
    async report() {
      asked += 1;
      return null;
    },
  });
  await route({ to: "worker", type: "state", state: state() }, 1, surface);
  assert.deepEqual(calls.offscreen, [
    { to: "offscreen", type: "enable", enabled: true },
  ]);
  assert.equal(asked, 0);
});

test("a local clipboard change is fanned out to every granted window", async () => {
  const { surface, calls } = fake({
    async grantedTabs() {
      return [4, 7];
    },
  });
  await route(
    { to: "worker", type: "clipboardLocal", text: "hi" },
    undefined,
    surface,
  );
  // Fanned out rather than addressed, because the worker does not know which window has
  // a live desktop and refuses to remember between messages.
  assert.deepEqual(calls.content, [
    {
      tabId: 4,
      message: { to: "content", type: "clipboardLocal", text: "hi" },
    },
    {
      tabId: 7,
      message: { to: "content", type: "clipboardLocal", text: "hi" },
    },
  ]);
});

test("a revoke says goodbye before it unregisters", async () => {
  const order: string[] = [];
  const { surface } = fake({
    async grantedTabs() {
      return [4];
    },
    async toContent(_tabId, message) {
      order.push(`bye:${(message as { type: string }).type}`);
    },
    async reconcile() {
      order.push("reconcile");
    },
    async paintAll() {
      order.push("paint");
    },
  });
  await route({ to: "worker", type: "revoked" }, undefined, surface);
  // The other order loses the goodbye for any window that has navigated since, and the
  // page is then left believing in a companion that has gone.
  assert.deepEqual(order, ["bye:bye", "reconcile", "paint"]);
});

test("describe answers with the pattern, the label and the grant", async () => {
  const report: PageReport = {
    state: state(),
    metrics: {
      innerWidth: 800,
      innerHeight: 600,
      outerWidth: 816,
      outerHeight: 700,
      availLeft: 0,
      availTop: 0,
      availWidth: 3000,
      availHeight: 2000,
    },
  };
  const { surface } = fake({
    async tabUrl() {
      return "https://gateway.example.com:8443/target/2";
    },
    async isGranted() {
      return true;
    },
    async report() {
      return report;
    },
  });
  assert.deepEqual(
    await route({ to: "worker", type: "describe", tabId: 1 }, 1, surface),
    {
      // The pattern loses the port and the label keeps it, which is what lets the popup
      // say the grant is wider than the host it names.
      pattern: "https://gateway.example.com/*",
      host: "gateway.example.com:8443",
      granted: true,
      report,
    },
  );
});

test("an ungranted window is described without asking the page anything", async () => {
  let asked = 0;
  const { surface } = fake({
    async tabUrl() {
      return "https://gateway.example.com/";
    },
    async report() {
      asked += 1;
      return null;
    },
  });
  const description = await route(
    { to: "worker", type: "describe", tabId: 1 },
    1,
    surface,
  );
  assert.equal(description?.granted, false);
  assert.equal(description?.report, null);
  assert.equal(asked, 0);
});

test("resize with no reported size moves no window", async () => {
  const { surface, calls } = fake({
    async report() {
      return {
        state: state({ size: null }),
        metrics: {
          innerWidth: 800,
          innerHeight: 600,
          outerWidth: 816,
          outerHeight: 700,
          availLeft: 0,
          availTop: 0,
          availWidth: 3000,
          availHeight: 2000,
        },
      };
    },
    async windowBounds() {
      return { x: 0, y: 0, width: 816, height: 700 };
    },
  });
  await route({ to: "worker", type: "resize", tabId: 1 }, 1, surface);
  assert.deepEqual(calls.bounds, []);
});

test("resize on a described desktop asks for the fitted bounds", async () => {
  const { surface, calls } = fake({
    async report() {
      return {
        state: state(),
        metrics: {
          innerWidth: 800,
          innerHeight: 600,
          outerWidth: 816,
          outerHeight: 700,
          availLeft: 0,
          availTop: 0,
          availWidth: 3000,
          availHeight: 2000,
        },
      };
    },
    async windowBounds() {
      return { x: 10, y: 20, width: 816, height: 700 };
    },
  });
  await route({ to: "worker", type: "resize", tabId: 1 }, 1, surface);
  assert.deepEqual(calls.bounds, [
    { tabId: 1, bounds: { x: 10, y: 20, width: 1936, height: 1180 } },
  ]);
});

test("the icon tells a tab apart from a site nobody turned on", async () => {
  const granted = {
    async isGranted() {
      return true;
    },
  };
  const url = {
    async tabUrl() {
      return "https://gateway.example.com/";
    },
  };

  assert.equal(
    await iconFor(
      1,
      fake({
        ...url,
        ...granted,
        async report() {
          return null;
        },
      }).surface,
    ),
    // Granted, but nothing answered: the script is injected in a tab too and only
    // listens in an app window.
    "not-app-window",
  );
  assert.equal(await iconFor(1, fake(url).surface), "not-granted");
});

test("waking spins the offscreen document up and paints the tab that woke it", async () => {
  const { surface, calls } = fake();
  await route({ to: "worker", type: "wake" }, 3, surface);
  assert.equal(calls.ensured, 1);
  assert.deepEqual(calls.painted, [3]);
});
