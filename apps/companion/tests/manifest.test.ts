// What this extension may do, pinned against a literal.
//
// A test that goes red the day somebody adds a permission, which is the point: the
// permissions are the whole of what a reader has to trust, and they should not be able
// to change without a deliberate edit here saying so.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const manifest = JSON.parse(
  await readFile(join(root, "src/manifest.json"), "utf8"),
) as Record<string, unknown>;

test("the permissions are exactly these five", () => {
  assert.deepEqual(manifest.permissions, [
    "scripting",
    "activeTab",
    "offscreen",
    "clipboardRead",
    "clipboardWrite",
  ]);
});

test("host access is optional, and there is none of it by default", () => {
  // The two broad patterns are what this extension may *ask* for. Granting is per site
  // and the user's, and `host_permissions` being absent is what makes that true — one
  // entry there would be access nobody was asked about.
  assert.deepEqual(manifest.optional_host_permissions, [
    "http://*/*",
    "https://*/*",
  ]);
  assert.equal("host_permissions" in manifest, false);
});

test("no content script is declared, because they are registered per grant", () => {
  // A `content_scripts` entry would run in every matching renderer whether the user
  // had turned the site on or not, which is the design this one replaced.
  assert.equal("content_scripts" in manifest, false);
});

test("nothing is reachable from a page, and nothing is probeable", () => {
  assert.equal("externally_connectable" in manifest, false);
  assert.equal("web_accessible_resources" in manifest, false);
});

test("the version is the build's to inject, not the repository's to state", () => {
  // One version for the whole repo, in Cargo.toml. A literal here would be a second
  // source of truth, and it would be the stale one.
  assert.equal("version" in manifest, false);
  assert.equal("version_name" in manifest, false);
});
