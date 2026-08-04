// Where a `remotex://app/...` request lands, before any file is touched.
//
// The traversal cases are the reason this is a function rather than a few lines
// inside the handler: a `..` that got through would hand out anything the user can
// read, and that is not something to find out about in a browser.

import { describe, expect, test } from "bun:test";
import {
  isShellUrl,
  resolveUnderRoot,
  routeShellRequest,
} from "../src/main/scheme-routes.ts";

const roots = { web: "/app/web", shell: "/app/shell" };

describe("routing", () => {
  test("the root is the client's document", () => {
    expect(routeShellRequest("remotex://app/", roots)).toEqual({
      file: "/app/web/index.html",
    });
  });

  test("an asset is served from the web root", () => {
    expect(
      routeShellRequest("remotex://app/assets/index-abc123.js", roots),
    ).toEqual({ file: "/app/web/assets/index-abc123.js" });
  });

  test("the shell's own pages come from beside them, not from the client", () => {
    expect(
      routeShellRequest("remotex://app/_shell/launch.html", roots),
    ).toEqual({
      file: "/app/shell/launch.html",
    });
  });

  test("another host is not this app's", () => {
    expect(routeShellRequest("remotex://elsewhere/index.html", roots)).toEqual({
      status: 404,
    });
  });

  test("a traversal never leaves the root", () => {
    // Two layers, and this asserts the first. `new URL` flattens dot segments for a
    // standard scheme — percent-encoded ones included — so what arrives is an
    // ordinary path that will simply not be there, rather than a way out.
    for (const path of [
      "/../../../etc/passwd",
      "/%2e%2e/%2e%2e/etc/passwd",
      "/assets/../../../etc/passwd",
      "/_shell/%2e%2e/%2e%2e/etc/passwd",
    ]) {
      const routed = routeShellRequest(`remotex://app${path}`, roots);
      expect("file" in routed ? routed.file : "").toStartWith("/app/");
    }
  });

  test("the guard itself refuses every way out", () => {
    // The second layer, which the parser above happens to make unreachable today.
    // It is asserted directly because "unreachable" is a property of somebody
    // else's parser, and this is the line that has to hold if that ever changes.
    for (const path of ["/../secret", "/a/../../secret", "/"]) {
      expect(resolveUnderRoot("/app/web", path)).toBeNull();
    }
    expect(resolveUnderRoot("/app/web", "/assets/app.js")).toBe(
      "/app/web/assets/app.js",
    );
  });

  test("a path that cannot be decoded is refused, not thrown", () => {
    // Both of these used to throw out of the handler rather than answer it — a
    // stray `%` inside `decodeURIComponent`, and a NUL inside every `fs` call that
    // would have touched the result. A rejected handler promise is a request that
    // hangs, which is the one failure with nothing to read anywhere.
    for (const path of ["/%", "/%zz/app.js", "/assets/%E0%A4%A", "/a%00b"]) {
      expect(resolveUnderRoot("/app/web", path)).toBeNull();
      expect(routeShellRequest(`remotex://app${path}`, roots)).toEqual({
        status: 403,
      });
      expect(routeShellRequest(`remotex://app/_shell${path}`, roots)).toEqual({
        status: 403,
      });
    }
  });
});

describe("what this window may load", () => {
  test("only the shell's own origin", () => {
    expect(isShellUrl("remotex://app/index.html")).toBe(true);
    expect(isShellUrl("remotex://app/_shell/config.html")).toBe(true);
  });

  test("everything else is refused, the gateway's own origin included", () => {
    // The page talks to the gateway over `fetch` and a socket; it never navigates
    // there, so a navigation to it is something going wrong.
    for (const url of [
      "http://127.0.0.1:49213/",
      "remotex://elsewhere/",
      "file:///etc/passwd",
      "https://example.com/",
      "javascript:alert(1)",
      "not a url",
    ]) {
      expect(isShellUrl(url)).toBe(false);
    }
  });
});
