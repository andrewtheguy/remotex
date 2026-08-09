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

test("the permissions are exactly these three", () => {
  assert.deepEqual(manifest.permissions, [
    "offscreen",
    "clipboardRead",
    "clipboardWrite",
  ]);
});

test("there is one host, it is hard-coded, and nothing may be asked for later", () => {
  // The whole access model, in two lines of JSON. `optional_host_permissions` being
  // absent is half of it: with none declared, there is no origin this extension can
  // come to hold that is not written here.
  assert.deepEqual(manifest.host_permissions, ["http://*.remotex.localhost/*"]);
  assert.equal("optional_host_permissions" in manifest, false);
});

test("the content script is declared, and only for that host", () => {
  // Declared rather than registered per grant: with one host there is nothing to
  // reconcile, and the matches must be the host permission exactly — a wider pattern
  // here would be a renderer this extension runs in without having said so above.
  assert.deepEqual(manifest.content_scripts, [
    {
      matches: ["http://*.remotex.localhost/*"],
      js: ["content.js"],
      run_at: "document_start",
      all_frames: false,
    },
  ]);
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
