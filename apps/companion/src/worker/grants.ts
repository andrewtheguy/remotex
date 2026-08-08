// Which sites this extension runs in, which is a question only Chrome can answer.
//
// There is no host list in this tree. `chrome.permissions.getAll()` **is** the list:
// the user grants an origin from the popup, Chrome stores the grant in the profile,
// and everything here is derived from reading it back. That is what makes the list
// survive an update — an unpacked extension has no auto-update, so its directory is
// overwritten by hand, and anything kept in there would go with it.
//
// Registered content scripts are *derived* state, not stored state. They are
// reconciled against the grants on every worker start rather than trusted to persist,
// because `persistAcrossSessions` is a promise about browser sessions and this also
// has to survive the extension being reloaded from `chrome://extensions`.

import { isOriginPattern } from "../shared/origin.ts";

/** One registration per origin, named after it, so reconciliation is a set difference. */
const PREFIX = "site:";

/** The origins Chrome has granted, as patterns. */
export async function grantedOrigins(): Promise<string[]> {
  const permissions = await chrome.permissions.getAll();
  return (permissions.origins ?? []).filter(isOriginPattern);
}

export async function isGranted(pattern: string | null): Promise<boolean> {
  if (!pattern) {
    return false;
  }
  return chrome.permissions.contains({ origins: [pattern] });
}

/**
 * Make the registered content scripts match the grants exactly.
 *
 * Both directions, and both matter: a script with no grant is one Chrome would refuse
 * to inject anyway, and a grant with no script is a site the user turned on that
 * silently does nothing.
 */
export async function reconcile(): Promise<void> {
  const wanted = new Set(await grantedOrigins());
  const registered = await chrome.scripting.getRegisteredContentScripts();
  const stale = registered
    .filter((script) => script.id.startsWith(PREFIX))
    .filter((script) => !wanted.has(script.id.slice(PREFIX.length)));
  if (stale.length > 0) {
    await chrome.scripting.unregisterContentScripts({
      ids: stale.map((script) => script.id),
    });
  }
  const already = new Set(registered.map((script) => script.id));
  const missing = [...wanted].filter((origin) => !already.has(PREFIX + origin));
  if (missing.length > 0) {
    await chrome.scripting.registerContentScripts(missing.map(registration));
  }
}

/**
 * Take up a new grant, and inject into the window that asked for it, once.
 *
 * It takes no origin, which is the design showing through: the grant is already in
 * `chrome.permissions` by the time the popup says anything, so reading it back is both
 * simpler and the only version that cannot disagree with Chrome.
 *
 * The injection is what makes a grant take effect without a reload:
 * `registerContentScripts` is for documents that have yet to load, and the window the
 * user just turned this on in is not one of them.
 */
export async function grant(tabId: number): Promise<void> {
  await reconcile();
  try {
    await chrome.scripting.executeScript({
      target: { tabId },
      files: ["content.js"],
    });
  } catch (error) {
    // A tab that navigated away between the grant and this call, or one Chrome will
    // not script. The registration stands, so the next load has it either way.
    console.warn("could not inject into the granted tab", error);
  }
}

function registration(
  origin: string,
): chrome.scripting.RegisteredContentScript {
  return {
    id: PREFIX + origin,
    matches: [origin],
    js: ["content.js"],
    runAt: "document_start",
    allFrames: false,
    persistAcrossSessions: true,
  };
}
