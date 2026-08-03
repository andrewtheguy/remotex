// `GET /api/config`, and the fallbacks that keep a failure here from becoming a failure
// somewhere louder.
//
// Its own file because the module memoizes one promise per page, deliberately — two
// callers want the same response — and a memo cannot be reset. So the cases that need it
// unresolved have to be the only ones that touch it, which is what a file boundary means
// to the test runner.

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

const globals = globalThis as unknown as {
  fetch: typeof fetch;
  /** Optional because this runtime is not a browser — `gateway.ts` needs one to exist. */
  window?: unknown;
};
const realFetch = globals.fetch;
const hadWindow = "window" in globals;
const realWindow = globals.window;

let fetches = 0;

beforeEach(() => {
  fetches = 0;
  globals.window = {
    location: { origin: "https://gateway.test" },
    isSecureContext: true,
  };
});

afterEach(() => {
  globals.fetch = realFetch;
  if (hadWindow) {
    globals.window = realWindow;
  } else {
    delete globals.window;
  }
});

test("an unreachable gateway answers with the fallback rather than rejecting", async () => {
  // The whole reason this never rejects: the codec probe awaits it before a connect, and
  // a rejection there would take the connect down over a branding request. An empty codec
  // list is also exactly what a browser with no decoder would have reported, so the
  // gateway's refusal path handles it already.
  globals.fetch = (async () => {
    fetches += 1;
    throw new TypeError("network down");
  }) as unknown as typeof fetch;

  const { gatewayConfig } = await import("./gatewayConfig.ts");
  const config = await gatewayConfig();
  assert.equal(config.branding, "remotex");
  assert.deepEqual(config.videoCodecs, []);
  assert.equal(config.protocolVersion, 0);

  // And memoized even in failure: one unreachable gateway is one request, not one per
  // caller. Identity, so this says nothing about how many callers there were.
  assert.equal(gatewayConfig(), gatewayConfig());
  assert.equal(fetches, 1, "a failed config request was retried per caller");
});
