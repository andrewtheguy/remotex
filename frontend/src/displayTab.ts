// The tab a display is shown in beside the session's page, found again by name.
//
// The session page opens it by a window name rather than as a new tab each time,
// so a second click on the link shows the tab already open instead of opening
// another, which would only be told the display is in use. A name is found from
// the page that opened the tab, and from that page reloaded; a `/display/N` the
// user opened by hand has no name and is not found.
//
// Finding a tab by name means opening it without `noopener`, and a tab opened so
// starts with a copy of this page's `sessionStorage`, the session token included,
// and a reference to this page. The display's page drops both as it loads
// (`standAlone`): it claims nothing and is let in by the login cookie alone. The
// reference is made again each time the tab is found, and dropped here then.
import { GATEWAY_ORIGIN, gatewayUrl } from "./gateway.ts";

/** The page of the display shown in tab `tab`. */
export function displayTabUrl(tab: number): string {
  return gatewayUrl(`/display/${tab}`);
}

/** The window name the tab showing display `tab` is opened under. */
export function displayTabName(tab: number): string {
  return `remotex-display-${tab}`;
}

/** As much of a window as finding a display's tab takes. */
export interface TabWindow {
  location: { origin: string; pathname: string; replace(url: string): void };
  opener: unknown;
  focus(): void;
}

/**
 * Bring the tab showing display `tab` to the front, opening it where there is
 * none. Called from a click, which is what lets a browser open and focus it.
 * False where the browser opened nothing.
 */
export function showDisplayTab(
  tab: number,
  open: (url: string, name: string) => TabWindow | null = (url, name) =>
    window.open(url, name),
): boolean {
  // No address: a tab of this name is returned as it is, not loaded again, and
  // a new one starts blank.
  const shown = open("", displayTabName(tab));
  if (!shown) {
    return false;
  }
  let there = false;
  try {
    there =
      shown.location.origin === GATEWAY_ORIGIN &&
      shown.location.pathname.replace(/\/$/, "") === `/display/${tab}`;
  } catch {
    // The tab was since taken to another site, whose address is not ours to read.
  }
  if (there) {
    // Finding the tab made this page its opener again, which its page let go of
    // as it loaded and does not load again to let go of now.
    shown.opener = null;
  } else {
    shown.location.replace(displayTabUrl(tab));
  }
  shown.focus();
  return true;
}

/**
 * On a display's page as it loads: let go of what the page that opened it left
 * with it, `keys` of its storage and the reference to it.
 */
export function standAlone(keys: string[]): void {
  try {
    for (const key of keys) {
      sessionStorage.removeItem(key);
    }
  } catch {
    // Storage blocked: nothing was copied into it either.
  }
  window.opener = null;
}
