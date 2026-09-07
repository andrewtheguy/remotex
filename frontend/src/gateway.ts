/// Where this client's gateway is, and how to call it.
///
/// The page is served by its gateway, so every request is same-origin. Keeping URL
/// construction here gives fetches, WebSockets, and assets one spelling of that
/// origin.
///
/// The document's own origin, or an empty string where there is no document.
///
/// Guarded because this module is imported by tests that run outside a browser,
/// and one that throws on the way in cannot be tested at all.
import type { VideoCodec } from "./videoCodec.ts";

const DOCUMENT_ORIGIN =
  typeof window === "undefined" ? "" : window.location.origin;

/// The gateway's origin, with no trailing slash.
export const GATEWAY_ORIGIN = DOCUMENT_ORIGIN.replace(/\/$/, "");

/// An absolute URL for a gateway path (`/api/targets`).
export function gatewayUrl(path: string): string {
  return `${GATEWAY_ORIGIN}${path}`;
}

/// `fetch` against the gateway.
///
/// `credentials: "include"` makes the session-cookie requirement explicit even
/// though same-origin fetches would send it by default.
export function gatewayFetch(
  path: string,
  init?: RequestInit,
): Promise<Response> {
  return fetch(gatewayUrl(path), { credentials: "include", ...init });
}

/// The WebSocket URL for `path`, carrying `session` as the claim.
///
/// Derived from the gateway's origin rather than the document's, for the same
/// reason as above — and the scheme follows it, so a gateway on `https:` gets
/// `wss:` whether or not the page itself was loaded over TLS.
///
/// The session socket also names this window's `screen` (the same numbers
/// `connect` carries), so a gateway holding a target whose engine a claim
/// change ended can reconnect it for this browser's screen at attach time —
/// before any message this client could send.
export function gatewaySocketUrl(
  path: string,
  session: string,
  client?: {
    screen: { w: number; h: number; scale: number; fit: boolean };
    video: VideoCodec;
  },
): string {
  const url = new URL(gatewayUrl(path));
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.search = `?session=${encodeURIComponent(session)}`;
  if (client) {
    url.searchParams.set("w", String(client.screen.w));
    url.searchParams.set("h", String(client.screen.h));
    url.searchParams.set("scale", String(client.screen.scale));
    url.searchParams.set("fit", String(client.screen.fit));
    // Which codec this browser's streams are to be encoded in, for the same reason
    // the screen is here: a takeover's reconnect happens at attach, before this
    // client could say anything, and the gateway must not have to guess.
    url.searchParams.set("video", client.video);
  }
  return url.toString();
}
