// Whether `GET /api/config` says `[airplay]` is set, as the version line reads it.
// Parsed directly rather than fetched: the module memoizes one promise per page,
// and gatewayConfig.test.ts is the one file that may resolve it.

import assert from "node:assert/strict";
import { test } from "node:test";
import { parseGatewayConfig } from "./gatewayConfig.ts";

test("airplay is set only when the gateway says true", () => {
  assert.equal(parseGatewayConfig({ airplay: true }).airplay, true);
  assert.equal(parseGatewayConfig({ airplay: false }).airplay, false);
  const truthy = { airplay: "yes" } as unknown;
  assert.equal(
    parseGatewayConfig(truthy as Parameters<typeof parseGatewayConfig>[0])
      .airplay,
    false,
  );
});

test("a gateway that does not say has no airplay", () => {
  assert.equal(parseGatewayConfig({}).airplay, false);
});
