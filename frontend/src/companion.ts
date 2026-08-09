// The seam between this SPA and the RemoteX Companion extension, which is how a page
// gets the two things `remotex.app` has and it does not: the system clipboard while
// the window is unfocused, and a window it can resize.
//
// **Only in an app window.** The extension does nothing in an ordinary tab and this
// seam settles to `absent` there without posting anything at all, which is not a
// limitation being worked around but the whole shape of the design: an app window is
// the configuration the client is meant to be run in, it is the one that keeps the
// browser's chords (see appWindow.ts), and serving one window kind properly beats
// serving two of them halfway. See docs/companion-extension.md.
//
// The difference from `nativeHost.ts` that shapes this whole file is **when the other
// side turns up**. The app's bridge is exposed by a preload that runs before any script
// in the document, so `NATIVE_HOST` is read once and is never wrong. Nothing sequences
// a content script against this bundle, and nothing tells a page synchronously that one
// was injected into it, so the only way to find out is to ask and wait. "Is there a
// companion?" arrives late. It is a store, not a constant.
//
// A settled answer can still be re-asked, and `pageshow` is the one thing that asks:
// a bfcache restore replays no injection, so the page may come back to an extension
// that is there or gone. What is *not* allowed is drifting back to `probing` on its
// own — only a `pageshow` does that, and it re-arms the deadline with it, so there is
// no phase left waiting for an answer nothing will settle.
//
// Nothing tells this page a companion has *left*. The extension serves one hard-coded
// host and asks for nothing at runtime, so there is no grant to be taken away mid-
// session; and one disabled or reloaded from chrome://extensions takes its content
// script's context with it before it could say anything. That leaves a page believing
// in a companion that has gone until it is reloaded.
//
// With no extension installed every export here is inert bar the deadline: `hello`
// goes out to a bus nobody is reading, the phase settles to `absent`, and the client
// carries on exactly as it did before this file existed.

import { useEffect, useSyncExternalStore } from "react";
import { appWindow, onAppWindowChange } from "./appWindow.ts";
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
 * push and in the other is a clipboard that never syncs, and because what is being
 * waited for is a content script that may not have run yet rather than any work it has
 * to do first.
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
 *
 * Asked again when a tab *becomes* an app window, which *Install page as app…* does to
 * this very document without reloading it. Read once, this seam would stay dead in the
 * window the install just opened.
 */
function possible(): boolean {
  return typeof window !== "undefined" && appWindow();
}

let snapshot: Snapshot = possible() ? INITIAL : ABSENT;
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
/**
 * Say hello and give the other side until the deadline to answer.
 *
 * The same three lines at load and on every `pageshow`, which is what keeps the second
 * one honest: a restore that re-opens the question has to re-arm the answer too, or a
 * page that came back to a companion that has since been removed would sit in
 * `probing` for the rest of its life with the clipboard reader stood down behind it.
 *
 * A seam that is already `connected` is not re-opened — the hello still goes out, so a
 * content script that came back with the page has its state re-posted, but there is no
 * answer being waited for and nothing to stand down.
 *
 * `asked` is what makes "re-arms" true rather than "arms another". Two `pageshow`
 * events inside one deadline would otherwise leave two timers running, and the first
 * would find the *second* probe's `probing` and call it absent early — cutting the new
 * question's answer short by however long ago the old one was asked.
 */
let asked = 0;

function probe(): void {
  if (snapshot.phase !== "connected") {
    settle(INITIAL);
  }
  const mine = ++asked;
  post({ type: "hello", client: "remotex" });
  setTimeout(() => {
    if (mine === asked && snapshot.phase === "probing") {
      settle(ABSENT);
    }
  }, HANDSHAKE_DEADLINE_MS);
}

function listen(): void {
  window.addEventListener("message", receive);
  probe();
  // A bfcache restore re-runs no content script. Ours may still be there and may not;
  // saying hello again is how we find out, and it costs one message.
  window.addEventListener("pageshow", probe);
}

if (possible()) {
  listen();
} else if (typeof window !== "undefined") {
  // The install case, and the only way out of `absent` a tab has. The extension's own
  // content script is already in this document — it is matched on the host, not on the
  // window kind — and it arms on the same signal, so both sides of the handshake wake
  // up together and either may be first.
  const stop = onAppWindowChange(() => {
    stop();
    listen();
  });
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

/**
 * The same answer without React, which is how the phases are told apart at all.
 *
 * `probing` and `absent` are both "no" to {@link postToCompanion}, and the difference
 * between them is the whole reason this is a three-state store rather than a boolean —
 * so something has to be able to see it. The client reads the phase through the hook;
 * this is for the tests that assert the transitions, and for reading one outside a
 * component.
 */
export function companionPhase(): CompanionPhase {
  return snapshot.phase;
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
