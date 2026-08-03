/// Where this client's gateway is, and how to call it.
///
/// A browser loads this page *from* its gateway, so the gateway is wherever the
/// document came from and every path can be relative. `remotex.app` loads the same
/// page as `remotex://app` out of its own bundle, which is a real origin but not
/// the gateway's — so it tells the page where the gateway is, once, before the
/// document runs: `window.__remotexGateway`, injected into `index.html` by the
/// scheme handler as it is served, ahead of the client's own script.
///
/// One code path, not two: the browser simply has no global set and falls through
/// to its own origin, which is what it always did. Nothing here branches on
/// `NATIVE_HOST` — a page that was handed a gateway uses it, and that is the whole
/// rule.
declare global {
  interface Window {
    /// Absolute origin of the gateway (`http://127.0.0.1:49213`), no trailing
    /// slash. Set by the native shell only.
    __remotexGateway?: string;
  }
}

/// Read once, at module load. The injected script is in the document ahead of this
/// one — it is written into `index.html` as the page is served — so this cannot
/// race it, and a value that changed later would leave sockets and fetches pointing
/// at different gateways.
const CONFIGURED =
  typeof window === "undefined" ? undefined : window.__remotexGateway;

/// The gateway's origin, with no trailing slash.
///
/// `location.origin` is the browser's answer, and the fallback. In the shell it is
/// `remotex://app`, which serves the client and nothing else — so if the shell ever
/// failed to set the global, every call below fails loudly at the first request
/// rather than quietly resolving to something wrong.
export const GATEWAY_ORIGIN = (CONFIGURED ?? window.location.origin).replace(
  /\/$/,
  "",
);

/// Whether this client is talking to a gateway it was *not* loaded from.
///
/// True only in the native shell. Exported for the one thing that has to know:
/// nothing about the UI, but the fact that requests are cross-origin and therefore
/// need credentials asked for explicitly.
export const CROSS_ORIGIN = CONFIGURED !== undefined;

/// An absolute URL for a gateway path (`/api/targets`).
export function gatewayUrl(path: string): string {
  return `${GATEWAY_ORIGIN}${path}`;
}

/// `fetch` against the gateway.
///
/// `credentials: "include"` on every call, and it is load-bearing exactly once: a
/// same-origin fetch sends the session cookie anyway, but a *cross-origin* one
/// omits it unless asked — so without this the native shell's requests arrive
/// authenticated-looking and anonymous, and the gateway answers 401 with nothing
/// on the wire to explain why. Harmless in a browser, which is why it is
/// unconditional rather than another branch.
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
