// The extension's own bus, and its guards.
//
// `chrome.runtime.sendMessage` delivers to every context at once — the worker, the
// offscreen document and an open popup all hear the same message. So the `to` field is
// the whole of what stops two contexts doing the same work, and these are the tests
// that keep it honest.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  isToContent,
  isToOffscreen,
  isToWorker,
} from "../src/shared/messages.ts";

test("a message is read only by the context it names", () => {
  const wake = { to: "worker", type: "wake" };
  assert.equal(isToWorker(wake), true);
  assert.equal(isToContent(wake), false);
  assert.equal(isToOffscreen(wake), false);

  // `clipboardLocal` is a type two contexts both have, going opposite ways: the
  // offscreen document sends one to the worker, and the worker sends one to a content
  // script. Without the address they would be the same message.
  const fromOffscreen = { to: "worker", type: "clipboardLocal", text: "x" };
  const toPage = { to: "content", type: "clipboardLocal", text: "x" };
  assert.equal(isToWorker(fromOffscreen), true);
  assert.equal(isToContent(fromOffscreen), false);
  assert.equal(isToContent(toPage), true);
  assert.equal(isToWorker(toPage), false);
});

test("an unknown type is refused even at the right address", () => {
  assert.equal(isToWorker({ to: "worker", type: "evict" }), false);
  assert.equal(isToContent({ to: "content", type: "wake" }), false);
  assert.equal(isToOffscreen({ to: "offscreen", type: "report" }), false);
});

test("a recognised type without its payload is refused too", () => {
  // The guards are type predicates, so passing one means the router may read
  // `message.tabId` as a number. A `describe` with none would reach `chrome.tabs.get`
  // as `undefined`; a `clipboardLocal` with none would post `undefined` at the page.
  assert.equal(isToWorker({ to: "worker", type: "describe" }), false);
  assert.equal(isToWorker({ to: "worker", type: "resize" }), false);
  assert.equal(isToWorker({ to: "worker", type: "granted" }), false);
  assert.equal(isToWorker({ to: "worker", type: "clipboardLocal" }), false);
  assert.equal(
    isToWorker({ to: "worker", type: "clipboardFromRemote" }),
    false,
  );
  assert.equal(isToWorker({ to: "worker", type: "state" }), false);
  assert.equal(isToContent({ to: "content", type: "clipboardLocal" }), false);
  assert.equal(isToOffscreen({ to: "offscreen", type: "enable" }), false);
  assert.equal(isToOffscreen({ to: "offscreen", type: "fromRemote" }), false);
});

test("a payload of the wrong type is refused as firmly as a missing one", () => {
  assert.equal(
    isToWorker({ to: "worker", type: "describe", tabId: "3" }),
    false,
  );
  assert.equal(isToWorker({ to: "worker", type: "resize", tabId: 1.5 }), false);
  assert.equal(
    isToWorker({ to: "worker", type: "granted", tabId: null }),
    false,
  );
  assert.equal(
    isToWorker({ to: "worker", type: "clipboardLocal", text: 7 }),
    false,
  );
  assert.equal(isToWorker({ to: "worker", type: "state", state: null }), false);
  assert.equal(
    isToWorker({ to: "worker", type: "state", state: "desktop" }),
    false,
  );
  assert.equal(
    isToOffscreen({ to: "offscreen", type: "enable", enabled: "yes" }),
    false,
  );
});

test("the payloads this extension actually sends are accepted", () => {
  assert.equal(isToWorker({ to: "worker", type: "describe", tabId: 3 }), true);
  assert.equal(isToWorker({ to: "worker", type: "granted", tabId: 0 }), true);
  // An empty string is a clipboard that was cleared, which is a value and not an
  // absence: the synchronizer decides what to do with it, not the guard.
  assert.equal(
    isToWorker({ to: "worker", type: "clipboardLocal", text: "" }),
    true,
  );
  assert.equal(isToWorker({ to: "worker", type: "state", state: {} }), true);
  assert.equal(
    isToOffscreen({ to: "offscreen", type: "enable", enabled: false }),
    true,
  );
});

test("the guards never throw and refuse everything unrecognised", () => {
  for (const guard of [isToWorker, isToContent, isToOffscreen]) {
    for (const value of [
      null,
      undefined,
      "worker",
      42,
      [],
      {},
      { type: "wake" },
      { to: "worker" },
      { to: "worker", type: 7 },
      // An address on a nested object is not an address on the message.
      { payload: { to: "worker", type: "wake" } },
    ]) {
      assert.equal(guard(value), false, `${JSON.stringify(value) ?? value}`);
    }
  }
});
