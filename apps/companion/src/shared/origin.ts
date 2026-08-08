// A tab's URL to the origin pattern this extension asks Chrome for.
//
// Pure, and the one place a mistake grants more than was meant, which is why it is its
// own module with its own table-driven test rather than three lines in the popup.
//
// There is no matcher here and there is no host list. Chrome holds the grants and
// Chrome decides what they match; all this does is turn "the window I am looking at"
// into the pattern to request for it.

/**
 * The pattern to ask for, or null if this is not a page a grant would mean anything on.
 *
 * Refused: everything that is not `http:` or `https:`. `chrome://`, `about:`, `file:`
 * and `data:` are not sites Chrome will grant, and a null here is what makes the popup
 * render its "nothing to enable" state rather than a switch that cannot be turned on.
 *
 * **The port is dropped, because a match pattern cannot express one.** A gateway on
 * `https://gateway.example:8443` is asked for as `https://gateway.example/*`, which
 * covers every port on that host. That is a real widening and it is stated in
 * docs/companion-extension.md; it is not something this function can fix, only
 * something it must not hide.
 *
 * The path is dropped too, and `/*` is always the path pattern: the client is a SPA
 * whose path changes under the content script, so a grant tied to one would come and
 * go as the user moved around it.
 */
export function originPatternFor(url: string | undefined): string | null {
  if (!url) {
    return null;
  }
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return null;
  }
  // `hostname`, not `host`: the latter carries the port. An IPv6 literal keeps its
  // brackets here, which is what Chrome's patterns want too.
  if (parsed.hostname === "") {
    return null;
  }
  return `${parsed.protocol}//${parsed.hostname}/*`;
}

/**
 * The host as the popup prints it, which is the host and its port.
 *
 * Deliberately *not* what {@link originPatternFor} returns. The pattern is what Chrome
 * is asked for and the label is what the user is looking at, and where those two
 * differ — a non-default port — showing the pattern would quietly claim the grant is
 * narrower than it is. Showing the real host and asking for the wider pattern is the
 * honest pair; the popup says which in its own words.
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

/** Whether a granted pattern is one this extension asked for, rather than a leftover. */
export function isOriginPattern(pattern: string): boolean {
  return /^https?:\/\/[^/*]+\/\*$/.test(pattern);
}
