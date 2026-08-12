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
export function gatewaySocketUrl(path: string, session: string): string {
  const url = new URL(gatewayUrl(path));
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.search = `?session=${encodeURIComponent(session)}`;
  return url.toString();
}
