// The seam between this SPA and the RemoteX Companion extension, which is how a page
// gets the two things `remotex.app` has and it does not: the system clipboard while
// the window is unfocused, and a window it can resize.
//
// **Only in an app window.** The extension does nothing in an ordinary tab and this
// seam settles to `absent` there without posting anything at all, which is not a
// limitation being worked around but the whole shape of the design: an app window is
// the configuration the client is meant to be run in, it is the one that keeps the
// browser's chords (see appWindow.ts), and it is the one with no toolbar — so the page
// is the only surface the extension has, and a tab is a case with nothing to serve.
// See docs/companion-extension.md.
//
// The difference from `nativeHost.ts` that shapes this whole file is **when the other
// side turns up**. The app's bridge is exposed by a preload that runs before any
// script in the document, so `NATIVE_HOST` is read once and is never wrong. The
// extension's content script has to read its whitelist out of `chrome.storage` first,
// which is asynchronous, and its site can be added to that whitelist mid-session — so
// "is there a companion?" is a value that arrives late and can change. It is a store,
// not a constant.
//
// The store is monotonic on purpose: `probing → connected | absent`, and `connected →
// absent` on `bye`, never back to `probing`. Nothing that has settled can un-settle,
// so no behaviour can flip twice.
//
// With no extension installed every export here is inert bar the deadline: `hello`
// goes out to a bus nobody is reading, the phase settles to `absent`, and the client
// carries on exactly as it did before this file existed.

import { useEffect, useSyncExternalStore } from "react";
import { appWindow } from "./appWindow.ts";
import {
  type CompanionCapabilities,
  type CompanionCommand,
  type CompanionCommandHandlers,
  type CompanionEvent,
  isExtMessage,
  type NativeState,
  PAGE_SOURCE,
} from "./companion.contract.ts";

export type {
  CompanionCapabilities,
  CompanionCommand,
  CompanionCommandHandlers,
  CompanionEvent,
} from "./companion.contract.ts";

/**
 * Whether a companion has answered yet.
 *
 * `probing` is the honest answer for the first moment of a page's life and is not the
 * same as `absent`: the one behaviour that hangs off this — the focus-driven clipboard
 * read — stands down while probing, because starting it and stopping it a quarter of a
 * second later would push the same text twice and put the browser's clipboard prompt
 * on screen for nothing.
 */
export type CompanionPhase = "probing" | "connected" | "absent";

/**
 * How long a silent bus is given before the page concludes there is no extension.
 *
 * Generous, because the cost of being wrong in one direction is a duplicate clipboard
 * push and in the other is a clipboard that never syncs. The content script's own work
 * before it can answer is one `chrome.storage.local` read.
 */
export const HANDSHAKE_DEADLINE_MS = 1_500;

interface Snapshot {
  phase: CompanionPhase;
  capabilities: CompanionCapabilities | null;
}

const INITIAL: Snapshot = { phase: "probing", capabilities: null };
const ABSENT: Snapshot = { phase: "absent", capabilities: null };

/**
 * Whether there is any point listening: an app window with a `window` to listen on.
 *
 * A tab is `absent` from the first render rather than after the deadline, which is the
 * one behavioural difference and it is the right way round — the focus-driven clipboard
 * read has nothing to stand down for there.
 */
const POSSIBLE = typeof window !== "undefined" && appWindow();

let snapshot: Snapshot = POSSIBLE ? INITIAL : ABSENT;
const listeners = new Set<() => void>();

// A new object only when something actually changed. `useSyncExternalStore` compares
// snapshots by identity, so returning a fresh object per read is the classic
// infinite-render bug; this is the other half of avoiding it.
function settle(next: Snapshot): void {
  if (
    next.phase === snapshot.phase &&
    next.capabilities === snapshot.capabilities
  ) {
    return;
  }
  snapshot = next;
  for (const notify of listeners) {
    notify();
  }
}

function post(event: CompanionEvent): void {
  window.postMessage({ source: PAGE_SOURCE, ...event }, window.location.origin);
}

function receive(event: MessageEvent): void {
  // Three guards, and all three matter. `source !== window` drops anything posted by
  // an iframe or an opener; `origin` drops a cross-origin poster; the tag check drops
  // every other conversation happening on this bus.
  if (event.source !== window || event.origin !== window.location.origin) {
    return;
  }
  if (!isExtMessage(event.data)) {
    return;
  }
  const command: CompanionCommand = event.data;
  if (command.type === "hello") {
    settle({ phase: "connected", capabilities: command.capabilities });
  } else if (command.type === "bye") {
    settle(ABSENT);
  }
  for (const handler of commandHandlers) {
    handler(command);
  }
}

const commandHandlers = new Set<(command: CompanionCommand) => void>();

// Installed at module load rather than from an effect, and that is what makes the
// handshake safe: the content script may post its `hello` while React is still
// rendering the first frame, and an effect would not be listening yet. It also means
// StrictMode's double mount re-reads a store that is already settled instead of
// restarting a probe.
if (POSSIBLE) {
  window.addEventListener("message", receive);
  post({ type: "hello", client: "remotex" });
  window.addEventListener("pageshow", () => {
    // A bfcache restore re-runs no content script. Ours may still be there and may
    // not; saying hello again is how we find out, and it costs one message.
    if (snapshot.phase !== "connected") {
      settle(INITIAL);
    }
    post({ type: "hello", client: "remotex" });
  });
  setTimeout(() => {
    if (snapshot.phase === "probing") {
      settle(ABSENT);
    }
  }, HANDSHAKE_DEADLINE_MS);
}

function subscribe(notify: () => void): () => void {
  listeners.add(notify);
  return () => {
    listeners.delete(notify);
  };
}

function getSnapshot(): Snapshot {
  return snapshot;
}

function getServerSnapshot(): Snapshot {
  return ABSENT;
}

/** Whether a companion has answered. See {@link CompanionPhase}. */
export function useCompanion(): CompanionPhase {
  return useSyncExternalStore(subscribe, getSnapshot, getServerSnapshot).phase;
}

/** What the connected companion says it is doing, or null while there is none. */
export function useCompanionCapabilities(): CompanionCapabilities | null {
  return useSyncExternalStore(subscribe, getSnapshot, getServerSnapshot)
    .capabilities;
}

/**
 * Send one event to the companion.
 *
 * Returns whether there was one to take it, which is not decoration: the clipboard
 * caller uses it to decide between this path and `navigator.clipboard`, and posting
 * into the void would make that decision unanswerable. Nothing is queued — a `state`
 * sent before the handshake has nobody to read it, and `useCompanionState` re-posts
 * the moment one connects.
 */
export function postToCompanion(event: CompanionEvent): boolean {
  if (snapshot.phase !== "connected") {
    return false;
  }
  post(event);
  return true;
}

/**
 * Post `state` to the companion whenever it changes, and once more when one connects.
 *
 * JSON-compared for the same reason `useNativeState` is: it is a dozen values that
 * change together, the object is rebuilt on every render, and a hand-written
 * dependency list goes stale the first time a field is added.
 */
export function useCompanionState(state: NativeState): void {
  const encoded = JSON.stringify(state);
  const phase = useCompanion();
  useEffect(() => {
    if (phase !== "connected") {
      return;
    }
    postToCompanion({
      type: "state",
      state: JSON.parse(encoded) as NativeState,
    });
  }, [encoded, phase]);
}

/**
 * Receive commands for as long as the caller is mounted.
 *
 * The subscription is torn down on unmount, which is what stops a clipboard arriving
 * from the extension driving a session that has already ended.
 */
export function useCompanionCommands(handlers: CompanionCommandHandlers): void {
  useEffect(() => {
    const handler = (command: CompanionCommand) => {
      // A command this build does not know is dropped rather than thrown: the client
      // and the extension ship together, so it can only mean a hand-typed one in the
      // developer tools.
      const entry = handlers[command.type] as
        | ((c: CompanionCommand) => void)
        | undefined;
      entry?.(command);
    };
    commandHandlers.add(handler);
    return () => {
      commandHandlers.delete(handler);
    };
  }, [handlers]);
}
