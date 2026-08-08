// Three answers, because they are three different things to tell somebody.

import assert from "node:assert/strict";
import { test } from "node:test";
import { iconStateFor } from "../src/worker/icon.ts";

test("the icon's three states", () => {
  assert.equal(iconStateFor({ granted: true, appWindow: true }), "on");

  // The distinction the title exists for: "you have not turned this site on", which the
  // popup fixes, against "this is a tab", which it cannot.
  assert.equal(
    iconStateFor({ granted: true, appWindow: false }),
    "not-app-window",
  );
  assert.equal(
    iconStateFor({ granted: false, appWindow: true }),
    "not-granted",
  );
  assert.equal(
    iconStateFor({ granted: false, appWindow: false }),
    "not-granted",
  );
});
