// `GET /api/config`, fetched once per page and shared.
//
// Two things want it and they want it at different moments: `App` reads the branding
// before login, and the codec probe (`videoCodecs.ts`) reads the video list before the
// first connect. A module-level promise is what keeps that one request rather than two,
// and what lets the probe start early without the two racing to be first.
//
// A public route, so it resolves before authentication — which is why the probe can be
// underway while somebody is still typing a password.

import { gatewayFetch } from "./gateway.ts";

/** One entry of `videoCodecs`: a family name, and a string to probe a decoder with. */
export interface VideoCodecOffer {
  /** `vp9` or `h264` — what goes back on `connect`. */
  name: string;
  /**
   * A representative WebCodecs configuration string, *not* the one a session will use:
   * the exact string depends on the picture size, which only the remote knows. The probe
   * decides the family and `videoFormat` names the configuration. See
   * `VideoCodec::probe` in src/config.rs, which explains why the entries are held at
   * comparable strictness.
   */
  probe: string;
}

export interface GatewayConfig {
  branding: string;
  protocolVersion: number;
  /** Best first: the gateway's own preference, which its `connect` also chooses by. */
  videoCodecs: VideoCodecOffer[];
}

const FALLBACK: GatewayConfig = {
  branding: "remotex",
  protocolVersion: 0,
  videoCodecs: [],
};

let pending: Promise<GatewayConfig> | null = null;

/**
 * The gateway's public config, fetched at most once.
 *
 * Never rejects. A gateway that cannot be reached is a page that is about to fail at
 * something more visible than its branding, and a probe that threw here would take the
 * connect down with it — so the fallback is "no branding, no codecs", and an empty codec
 * list is exactly what a browser with no decoder would have reported anyway.
 */
export function gatewayConfig(): Promise<GatewayConfig> {
  pending ??= gatewayFetch("/api/config")
    .then((res) => res.json() as Promise<Partial<GatewayConfig>>)
    .then((config) => ({
      branding: config.branding || FALLBACK.branding,
      protocolVersion: config.protocolVersion ?? FALLBACK.protocolVersion,
      // Guarded rather than trusted: this is the one field a gateway older than this
      // client omits, and `[]` from it means "offer nothing", which the connect path
      // already handles by refusing a video target with a named error.
      videoCodecs: Array.isArray(config.videoCodecs) ? config.videoCodecs : [],
    }))
    .catch(() => FALLBACK);
  return pending;
}
