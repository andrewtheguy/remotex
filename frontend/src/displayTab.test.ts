import assert from "node:assert/strict";
import { test } from "node:test";
import {
  displayTabName,
  displayTabUrl,
  showDisplayTab,
  type TabWindow,
} from "./displayTab.ts";
import { GATEWAY_ORIGIN } from "./gateway.ts";

function tabAt(origin: string, pathname: string, search = "") {
  const calls: string[] = [];
  const tab: TabWindow = {
    location: {
      origin,
      pathname,
      search,
      replace: (url) => calls.push(`replace ${url}`),
    },
    opener: "session page",
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
  // Its page lets go of the opener as it loads.
  assert.equal(tab.opener, "session page");
});

test("a tab already showing the display is focused and not loaded again", () => {
  for (const path of ["/display/2", "/display/2/"]) {
    const { tab, calls } = tabAt(GATEWAY_ORIGIN, path);
    assert.equal(
      showDisplayTab(2, () => tab),
      true,
    );
    assert.deepEqual(calls, ["focus"]);
    assert.equal(tab.opener, null);
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
      search: "",
      replace: (url) => calls.push(`replace ${url}`),
    },
    opener: "session page",
    focus: () => calls.push("focus"),
  };
  assert.equal(
    showDisplayTab(2, () => tab),
    true,
  );
  assert.deepEqual(calls, [`replace ${GATEWAY_ORIGIN}/display/2`, "focus"]);
});

test("a display's tab carries the session page's software decoder switches", () => {
  const scope = globalThis as unknown as { location?: { search: string } };
  const before = Object.getOwnPropertyDescriptor(scope, "location");
  Object.defineProperty(scope, "location", {
    value: { search: "?vp9_decoder=software&other=1&hevc_decoder=native" },
    configurable: true,
  });
  try {
    const url = `${GATEWAY_ORIGIN}/display/2?vp9_decoder=software`;
    assert.equal(displayTabUrl(2), url);
    // A tab opened without the switch decodes otherwise, so it is loaded again;
    const { tab, calls } = tabAt(GATEWAY_ORIGIN, "/display/2");
    showDisplayTab(2, () => tab);
    assert.deepEqual(calls, [`replace ${url}`, "focus"]);
    // one opened with it is only brought forward.
    const same = tabAt(GATEWAY_ORIGIN, "/display/2", "?vp9_decoder=software");
    showDisplayTab(2, () => same.tab);
    assert.deepEqual(same.calls, ["focus"]);
  } finally {
    if (before) {
      Object.defineProperty(scope, "location", before);
    } else {
      delete scope.location;
    }
  }
});

test("nothing opened is said so", () => {
  assert.equal(
    showDisplayTab(2, () => null),
    false,
  );
});
