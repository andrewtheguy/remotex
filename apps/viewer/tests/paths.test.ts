// Where the bundle's own files are, which is a question this app got wrong once and
// paid for in a way that did not look like an error.
//
// The preload is the only thing that tells the client which host it is in. Point at
// it wrongly and the page loses `__remotexGateway` and `remotexNative`, concludes it
// is an ordinary browser tab, and renders the login screen — for a gateway whose
// address it was never given. Nothing crashes; the app simply asks for a password
// that does not exist.

import { describe, expect, test } from "bun:test";
import { distDirFor, resourceLayout } from "../src/main/paths.ts";

describe("finding the bundle", () => {
  test("packaged, the bundle is dist/ inside the asar", () => {
    expect(
      distDirFor("/Applications/remotex.app/Contents/Resources/app.asar"),
    ).toBe("/Applications/remotex.app/Contents/Resources/app.asar/dist");
  });

  test("run from disk, the entry file's own directory is it", () => {
    expect(distDirFor("/repo/apps/viewer/dist")).toBe("/repo/apps/viewer/dist");
  });

  test("it is never the source tree", () => {
    // The bug: bun resolves `__dirname` at bundle time, so it named the directory
    // the *source* was in. Nothing in the answer may come from there.
    for (const appPath of [
      "/Applications/remotex.app/Contents/Resources/app.asar",
      "/repo/apps/viewer/dist",
      "/repo/apps/viewer",
    ]) {
      expect(distDirFor(appPath)).not.toContain("/src/");
    }
  });
});

describe("finding the gateway and the client", () => {
  test("packaged, both sit beside the app in Resources", () => {
    const layout = resourceLayout(
      true,
      "/Applications/remotex.app/Contents/Resources",
      "/Applications/remotex.app/Contents/Resources/app.asar/dist",
      {},
    );
    expect(layout.binary).toBe(
      "/Applications/remotex.app/Contents/Resources/remotex-gateway",
    );
    expect(layout.webRoot).toBe(
      "/Applications/remotex.app/Contents/Resources/web",
    );
  });

  test("run from disk, both come from where the repo builds them", () => {
    // Release, not debug: an unpackaged run is a manual QA run, and a debug build
    // of the encoders is too slow to judge a remote desktop by.
    const layout = resourceLayout(false, "/repo", "/repo/apps/viewer/dist", {});
    expect(layout.binary).toBe("/repo/target/release/remotex");
    expect(layout.webRoot).toBe("/repo/frontend/dist");
  });

  test("the environment wins over both", () => {
    const layout = resourceLayout(true, "/Resources", "/Resources/dist", {
      REMOTEX_GATEWAY_BIN: "/tmp/my-gateway",
      REMOTEX_WEB_ROOT: "/tmp/my-web",
    });
    expect(layout.binary).toBe("/tmp/my-gateway");
    expect(layout.webRoot).toBe("/tmp/my-web");
  });

  test("an empty override is not an override", () => {
    // `REMOTEX_GATEWAY_BIN=` is how a shell unsets what it exported a moment ago,
    // and an empty string is not nullish — so taken literally the app spawns `""`
    // and reports a bundle problem it does not have.
    const layout = resourceLayout(true, "/Resources", "/Resources/dist", {
      REMOTEX_GATEWAY_BIN: "",
      REMOTEX_WEB_ROOT: "   ",
    });
    expect(layout.binary).toBe("/Resources/remotex-gateway");
    expect(layout.webRoot).toBe("/Resources/web");
  });

  test("the shell's own pages always travel with the bundle", () => {
    // Never overridable and never in Resources: they are the screen that explains a
    // gateway that would not start, so they cannot depend on one.
    expect(
      resourceLayout(true, "/Resources", "/Resources/dist", {
        REMOTEX_WEB_ROOT: "/tmp/my-web",
      }).shellRoot,
    ).toBe("/Resources/dist/shell");
  });
});
