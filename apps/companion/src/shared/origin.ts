// The one host this extension serves, in the one place that decides it.
//
// There is no grant flow, no popup switch and no host list to keep: the manifest
// declares `http://*.remotex.localhost/*` as a host permission and as the content
// script's `matches`, and this module is the same rule written for code that has a URL
// in hand rather than a match pattern. The two must agree — Chrome decides where the
// content script runs, and this decides what the icon and the popup say about it, so a
// disagreement is an extension that works somewhere it claims not to or the reverse.
//
// `[server].dev_subdomain` in a gateway's config is what puts a browser there: it
// redirects any loopback name to `<label>.remotex.localhost`, keeping the port. That is
// the whole of the setup, and it is why the host can be hard-coded at all.

/** The match pattern in the manifest, repeated here so a reader sees both at once. */
export const COMPANION_MATCH = "http://*.remotex.localhost/*";

/** What the popup and the icon call it, and the only host in this tree. */
export const COMPANION_HOST = "remotex.localhost";

/**
 * Whether this is a URL the companion serves.
 *
 * `http:` only, because the gateway has no TLS listener and the dev redirect always
 * sends a browser to `http://`. A `.remotex.localhost` name is loopback by RFC 6761 and
 * a secure context by the same rule the client's preflight relies on, so there is
 * nothing `https:` would add here beyond a second pattern to keep in step.
 *
 * The bare `remotex.localhost` counts, because Chrome's `*.remotex.localhost` matches
 * the domain as well as its subdomains — the predicate has to say what the manifest
 * says. A port is ignored, since a match pattern cannot express one, so every port on
 * these names is served.
 *
 * **This is where the bridge may run, not who is on the other end.** RFC 6761 reserves
 * the name to loopback; it does not reserve it to this project, and any local process
 * that binds a port answers there too. Nothing here authenticates anything, and nothing
 * should be built on the idea that it does — docs/companion-extension.md states the
 * cost under Costs.
 */
export function isCompanionUrl(url: string | undefined): boolean {
  if (!url) {
    return false;
  }
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  if (parsed.protocol !== "http:") {
    return false;
  }
  // Already lower-cased by the URL parser, and an IPv6 literal keeps its brackets —
  // neither of which can ever end in this suffix, which is the answer wanted there.
  return (
    parsed.hostname === COMPANION_HOST ||
    parsed.hostname.endsWith(`.${COMPANION_HOST}`)
  );
}

/**
 * The host as the popup prints it, which is the host and its port.
 *
 * The port is what tells two gateways apart on one machine — the label in the hostname
 * is only there to give each of them a cookie origin — so the popup shows both.
 */
export function hostLabelFor(url: string | undefined): string | null {
  if (!url) {
    return null;
  }
  try {
    return new URL(url).host || null;
  } catch {
    return null;
  }
}
