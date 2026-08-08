// The offscreen document, created at most once.
//
// It is the only context in an extension that may touch the system clipboard while no
// window is focused, which is the whole reason this extension exists. A service worker
// cannot: it has no document, so no `execCommand` and no focused selection, and
// `navigator.clipboard` is not available to it either.

let creating: Promise<void> | null = null;

/**
 * Ensure the document exists, tolerating two callers at once.
 *
 * `createDocument` throws if a second call arrives while the first is still running,
 * and two windows loading together is exactly that. The shared promise is the guard,
 * and clearing it in `finally` rather than on success is what stops one failure
 * poisoning every later call.
 */
export async function ensureOffscreen(): Promise<void> {
  const existing = await chrome.runtime.getContexts({
    contextTypes: [chrome.runtime.ContextType.OFFSCREEN_DOCUMENT],
  });
  if (existing.length > 0) {
    return;
  }
  creating ??= chrome.offscreen
    .createDocument({
      url: "offscreen.html",
      reasons: [chrome.offscreen.Reason.CLIPBOARD],
      justification:
        "Reads and writes the system clipboard while the remote desktop window is unfocused.",
    })
    .finally(() => {
      creating = null;
    });
  await creating;
}
