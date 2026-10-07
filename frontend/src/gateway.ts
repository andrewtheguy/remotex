import type { VideoChroma } from "./videoChroma.ts";

/// Where this client's gateway is, and how to call it.
///
/// The page is served by its gateway, so every request is same-origin. Keeping URL
/// construction here gives fetches, WebSockets, and assets one spelling of that
/// origin.
///
/// The document's own origin — in one of the page's workers, the origin its script
/// was served from, which is the same one — or an empty string outside a browser.
///
/// Guarded because this module is imported by tests that run outside a browser,
/// and one that throws on the way in cannot be tested at all.
const DOCUMENT_ORIGIN = globalThis.location?.origin ?? "";

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
/// The session socket also names what only this window knows about itself: its
/// `screen` (the same numbers `connect` carries), the chroma its video decoder
/// takes, whether it decodes a High Performance Mac's picture, whether it composes
/// an RDP host's graphics pipeline, and whether it decodes the H.264 such a
/// pipeline may carry. All are here for one reason — a
/// gateway holding a target whose engine a claim change ended reconnects it at
/// attach time, before any message this client could send, and it must build that
/// session for this browser rather than the previous one, or cover it where the
/// session was started with a stream this one cannot take. The media sockets carry
/// the claim and nothing else.
export function gatewaySocketUrl(
  path: string,
  session: string,
  client?: {
    screen: { w: number; h: number; scale: number; fit: boolean };
    chroma: VideoChroma;
    appleMedia: boolean;
    rdpGraphics: boolean;
    rdpH264: boolean;
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
    url.searchParams.set("chroma", client.chroma);
    url.searchParams.set("apple_media", String(client.appleMedia));
    url.searchParams.set("rdp_graphics", String(client.rdpGraphics));
    url.searchParams.set("rdp_h264", String(client.rdpH264));
  }
  return url.toString();
}

/// The WebSocket URL of display `display`'s socket: its picture, and the input
/// made over it.
///
/// No claim rides on it. The display socket attaches by the login cookie, which
/// a page of this browser carries and nothing else does — and which is all a
/// display opened in another tab has: that tab is given no session token. What
/// such a tab presents is `tab`, its own name for itself, which tells its reload
/// from another tab, and `takeover` once its user has confirmed taking the
/// display from the tab showing it.
export function gatewayDisplaySocketUrl(
  display: number,
  tab: { id: string; takeover: boolean } | null = null,
): string {
  const url = new URL(gatewayUrl("/ws/display"));
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("display", String(display));
  if (tab) {
    url.searchParams.set("tab", tab.id);
    if (tab.takeover) {
      url.searchParams.set("takeover", "true");
    }
  }
  return url.toString();
}

/// The software HEVC decoder's files, `hevc.js` (wasm-bindgen's glue) and
/// `hevc.wasm`: a release of andrewtheguy/hevc-wasm the gateway serves beside
/// the bundle when it has the release archive. The decode worker and each
/// thread of the decoder's pool import the glue by this URL, and the glue is
/// given the module's.
export function hevcDecoderUrl(file: "hevc.js" | "hevc.wasm"): string {
  return gatewayUrl(`/hevc/${file}`);
}
