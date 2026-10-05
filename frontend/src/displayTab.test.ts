import assert from "node:assert/strict";
import { test } from "node:test";
import {
  displayTabName,
  showDisplayTab,
  type TabWindow,
} from "./displayTab.ts";
import { GATEWAY_ORIGIN } from "./gateway.ts";

function tabAt(origin: string, pathname: string) {
  const calls: string[] = [];
  const tab: TabWindow = {
    location: {
      origin,
      pathname,
      replace: (url) => calls.push(`replace ${url}`),
    },
    focus: () => calls.push("focus"),
  };
  return { tab, calls };
}

test("a new tab is opened by name, loaded and focused", () => {
  const { tab, calls } = tabAt("null", "blank");
  const opened: string[][] = [];
  const shown = showDisplayTab(2, (url, name) => {
    opened.push([url, name]);
    return tab;
  });
  assert.equal(shown, true);
  assert.deepEqual(opened, [["", displayTabName(2)]]);
  assert.deepEqual(calls, [`replace ${GATEWAY_ORIGIN}/display/2`, "focus"]);
});

test("a tab already showing the display is focused and not loaded again", () => {
  for (const path of ["/display/2", "/display/2/"]) {
    const { tab, calls } = tabAt(GATEWAY_ORIGIN, path);
    assert.equal(
      showDisplayTab(2, () => tab),
      true,
    );
    assert.deepEqual(calls, ["focus"]);
  }
});

test("a tab taken to another site is brought back", () => {
  const calls: string[] = [];
  const tab: TabWindow = {
    location: {
      get origin(): string {
        throw new Error("cross-origin");
      },
      pathname: "",
      replace: (url) => calls.push(`replace ${url}`),
    },
    focus: () => calls.push("focus"),
  };
  assert.equal(
    showDisplayTab(2, () => tab),
    true,
  );
  assert.deepEqual(calls, [`replace ${GATEWAY_ORIGIN}/display/2`, "focus"]);
});

test("nothing opened is said so", () => {
  assert.equal(
    showDisplayTab(2, () => null),
    false,
  );
});
