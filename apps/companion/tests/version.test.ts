// Cargo's version is semver; Chrome's is one to four small integers and nothing else.

import assert from "node:assert/strict";
import { test } from "node:test";
import { chromeVersion } from "../src/shared/version.ts";

test("an ordinary version passes through, and is named as well", () => {
  assert.deepEqual(chromeVersion("0.0.132"), {
    version: "0.0.132",
    version_name: "0.0.132",
  });
});

test("a pre-release keeps its name and loses its tag from the number", () => {
  // `0.9.0-rc.1` is a perfectly good Cargo version and an illegal Chrome one, so the
  // numeric head becomes the version and the whole string becomes what the user reads.
  assert.deepEqual(chromeVersion("0.9.0-rc.1"), {
    version: "0.9.0",
    version_name: "0.9.0-rc.1",
  });
});

test("build metadata is stripped, and does not become a field", () => {
  // Semver orders them `1.2.3-rc.1+build.5`, so the `+` has to go first or a version
  // carrying both keeps `+build` and turns it into a fourth number.
  assert.deepEqual(chromeVersion("1.2.3+build.1"), {
    version: "1.2.3",
    version_name: "1.2.3+build.1",
  });
  assert.equal(chromeVersion("1.2.3-rc.1+build.5").version, "1.2.3");
});

test("more than four fields is trimmed to four", () => {
  assert.equal(chromeVersion("1.2.3.4.5").version, "1.2.3.4");
});

test("anything Chrome would reject throws here instead", () => {
  for (const bad of [
    "",
    "x.y.z",
    "1.-2.3",
    "1.65536.0",
    "-rc.1",
    // Digits followed by anything else. `parseInt` reads these as 1, 2 and 3 and stops,
    // which would have shipped a manifest claiming a version nobody wrote.
    "1x.2.3",
    "1.2rc.3",
    "1.2.3beta",
    "1. 2.3",
    "1..3",
    "1.2.+3",
  ]) {
    assert.throws(() => chromeVersion(bad), `${bad} should throw`);
  }
});
