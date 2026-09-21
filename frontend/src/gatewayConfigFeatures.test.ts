// The features `GET /api/config` names, as the version line reads them. Parsed
// directly rather than fetched: the module memoizes one promise per page, and
// gatewayConfig.test.ts is the one file that may resolve it.

import assert from "node:assert/strict";
import { test } from "node:test";
import { parseGatewayConfig } from "./gatewayConfig.ts";

test("the features are the strings the gateway named, and nothing else it sent", () => {
  const body = { features: ["apple-hp-audio", 7, null] } as unknown;
  assert.deepEqual(
    parseGatewayConfig(body as Parameters<typeof parseGatewayConfig>[0])
      .features,
    ["apple-hp-audio"],
  );
});

test("a gateway that names no features has none", () => {
  assert.deepEqual(parseGatewayConfig({}).features, []);
});
