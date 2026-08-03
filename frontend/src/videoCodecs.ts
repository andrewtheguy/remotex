// The client's half of the video codec negotiation.
//
// The gateway can send VP9 or H.264 and prefers VP9 — because a stock Chromium carries
// it and H.264 is the one codec a browser is not guaranteed to have, which is why
// `remotex.app` could not decode a video target at all before this. Which one a session
// uses is decided by asking this browser, before it connects: `/api/config` publishes
// the gateway's ordered list with a string to probe each with, this asks
// `VideoDecoder.isConfigSupported` about them, and the accepted names ride on
// `connect`. The gateway then picks its own first preference from that list.
//
// Probed once per page and memoized. `App` starts it as it mounts rather than this module
// starting it on import, so that the round trip overlaps with the login the user is still
// finishing and the click that picks a target is not the thing that waits for it — and so
// that importing this module does no work, which is what makes it testable.
//
// An empty answer is not an error here. It is what a page on an insecure origin
// reports — `VideoDecoder` is secure-context only — and the gateway answers it by
// refusing a target that streams video, by name, while still connecting every target
// that does not.

import { gatewayConfig, type VideoCodecOffer } from "./gatewayConfig.ts";

let pending: Promise<string[]> | null = null;

/**
 * The codec families this browser accepted, in the gateway's order of preference.
 *
 * Memoized, and this is the function everything but the tests calls. Never rejects:
 * every failure — no `VideoDecoder`, a config the browser refuses, a gateway that offered
 * nothing — is an accepted list with fewer entries in it.
 */
export function acceptedVideoCodecs(): Promise<string[]> {
  pending ??= gatewayConfig().then(({ videoCodecs }) =>
    probeVideoCodecs(videoCodecs),
  );
  return pending;
}

/**
 * Ask this browser about each offer, keeping the ones it accepted in the order given.
 *
 * Separate from the memo above, and exported, because the memo is one line and the
 * decision is the part worth testing: a test that went through `acceptedVideoCodecs`
 * would exercise the cache from its second case onward and prove nothing about the fifth.
 */
export async function probeVideoCodecs(
  offers: VideoCodecOffer[],
): Promise<string[]> {
  if (typeof VideoDecoder === "undefined" || !VideoDecoder.isConfigSupported) {
    // Not a secure context, or a runtime with no WebCodecs. `videoUnavailable` in
    // videoDecoder.ts says which, where it matters; here they are the same answer.
    return [];
  }
  const accepted: string[] = [];
  for (const offer of offers) {
    // Sequential rather than `Promise.all`, deliberately: the list has two entries, the
    // answers come from a table in the browser, and preserving the gateway's order in
    // the result costs nothing this way. `supported` is a *promise* that rejects on a
    // malformed string, which is why each is caught on its own — one unparseable offer
    // from a newer gateway must not lose the others.
    try {
      const { supported } = await VideoDecoder.isConfigSupported({
        codec: offer.probe,
      });
      if (supported) {
        accepted.push(offer.name);
      }
    } catch {
      // A string this browser could not even parse. Not accepted, nothing said: the
      // gateway's refusal message is where a user learns that nothing was.
    }
  }
  return accepted;
}
