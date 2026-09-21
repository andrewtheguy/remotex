// `GET /api/config`, fetched once per page and shared.
//
// A public route, so it resolves before authentication — which is what lets `App` put the
// deployment's branding on the login screen. A module-level promise keeps it one request
// however many callers there turn out to be.

import { gatewayFetch } from "./gateway.ts";

export interface GatewayConfig {
  branding: string;
  /** Whether the gateway serves an icon at `/api/logo`. */
  logo: boolean;
  /** Whether the gateway records throughput `GET /api/throughput` can read. */
  throughput: boolean;
  /** The optional cargo features the gateway was built with, for the version line. */
  features: string[];
}

const FALLBACK: GatewayConfig = {
  branding: "remotex",
  logo: false,
  throughput: false,
  features: [],
};

/** The config in a response body, holding each key to its type and its fallback. */
export function parseGatewayConfig(
  config: Partial<GatewayConfig>,
): GatewayConfig {
  return {
    branding: config.branding || FALLBACK.branding,
    logo: config.logo === true,
    throughput: config.throughput === true,
    features: Array.isArray(config.features)
      ? config.features.filter((f) => typeof f === "string")
      : [],
  };
}

let pending: Promise<GatewayConfig> | null = null;

/**
 * The gateway's public config, fetched at most once.
 *
 * Never rejects. A gateway that cannot be reached is a page that is about to fail at
 * something more visible than its branding, so the fallback is the default branding and
 * nothing here handles an error.
 */
export function gatewayConfig(): Promise<GatewayConfig> {
  pending ??= gatewayFetch("/api/config")
    .then((res) => res.json() as Promise<Partial<GatewayConfig>>)
    .then(parseGatewayConfig)
    .catch(() => FALLBACK);
  return pending;
}
