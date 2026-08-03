/// Where this client's gateway is, and how to call it.
///
/// The page is loaded *from* its gateway, so the gateway is wherever the document
/// came from. Every URL the client uses is built here rather than inline, so the
/// answer is given in one place — including for the WebSockets, whose scheme has
/// to follow the document's rather than be assumed.

/// The gateway's origin, with no trailing slash.
export const GATEWAY_ORIGIN = window.location.origin.replace(/\/$/, "");

/// An absolute URL for a gateway path (`/api/targets`).
export function gatewayUrl(path: string): string {
  return `${GATEWAY_ORIGIN}${path}`;
}

/// `fetch` against the gateway.
///
/// `credentials: "include"` rather than the same-origin default, so that a session
/// cookie is sent on every call whatever the caller passed in `init` — an `init`
/// that sets `credentials` would otherwise silently drop it.
export function gatewayFetch(
  path: string,
  init?: RequestInit,
): Promise<Response> {
  return fetch(gatewayUrl(path), { credentials: "include", ...init });
}

/// The WebSocket URL for `path`, carrying `session` as the claim.
///
/// The scheme follows the document's, so a gateway on `https:` gets `wss:`.
export function gatewaySocketUrl(path: string, session: string): string {
  const url = new URL(gatewayUrl(path));
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.search = `?session=${encodeURIComponent(session)}`;
  return url.toString();
}
