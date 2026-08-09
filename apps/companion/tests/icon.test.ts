// Three answers, because they are three different things to tell somebody.

import assert from "node:assert/strict";
import { test } from "node:test";
import { iconStateFor } from "../src/worker/icon.ts";

test("the icon's three states", () => {
  assert.equal(iconStateFor({ served: true, appWindow: true }), "on");

  // The distinction the title exists for: "this is a tab", which nothing in the popup
  // can fix, against "this window is not a gateway of ours", which is an address.
  assert.equal(
    iconStateFor({ served: true, appWindow: false }),
    "not-app-window",
  );
  assert.equal(iconStateFor({ served: false, appWindow: true }), "elsewhere");
  assert.equal(iconStateFor({ served: false, appWindow: false }), "elsewhere");
});
