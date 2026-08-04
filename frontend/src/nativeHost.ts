// The seam between this SPA and `remotex.app`, which shows it in a window of its
// own.
//
// The app is a *shell*: it owns the window, the menu bar, the macOS pasteboard and
// the menu bar's claim on ⌘ chords, and nothing else. Everything below the title bar
// is this client, the same build a browser loads, talking to the same gateway over
// the same socket. So the seam carries only what a page cannot do for itself:
//
//   app → page   the local pasteboard, and the menu commands that stand in for the
//                floating menu the shell hides;
//   page → app   one state object the menus derive their titles, ticks and
//                enablement from, plus the remote's clipboard when it changes.
//
// Keys are **not** on this seam. The shell hands the keyboard over by dropping its
// own menu accelerators while a live desktop has focus, so ⌘W, ⌘Q and ⌘T arrive in
// this page as ordinary `keydown` events and take the same path every other key
// takes. There is one keyboard path, and it is the client's.
//
// Nothing about the session crosses it either. The app holds no claim, no socket and
// no wire format — which is the point: a protocol change is a change to this client
// and to nothing else.
//
// In a browser every export here is inert: `NATIVE_HOST` is false, `postToHost`
// returns, and no command handler is ever installed.

import { useEffect } from "react";
import type {
  NativeCommand,
  NativeCommandHandlers,
  NativeEvent,
  NativeState,
} from "./nativeHost.contract.ts";

// The payload types live next door, in a file with no React in it or under it, so
// the shell can type-check against them without the client's dependencies. They are
// re-exported here because this is where the client imports them from, and moving
// them was not meant to be a change anybody has to notice.
export type {
  HostRemoteSize,
  NativeCommand,
  NativeCommandHandlers,
  NativeEvent,
  NativeState,
} from "./nativeHost.contract.ts";

/**
 * The object `remotex.app`'s preload exposes on `window`, and the whole of the
 * shell's API to this page.
 *
 * Two methods, one per direction. Both carry structured values rather than strings:
 * the shell moves them over its own IPC, which clones them, so the types below are
 * the contract at both ends and nothing re-encodes them on the way.
 */
interface NativeHostBridge {
  post: (event: NativeEvent) => void;
  /** Subscribe to commands; the returned function detaches the listener. */
  onCommand: (handler: (command: NativeCommand) => void) => () => void;
}

/**
 * Whether this page is running inside `remotex.app` rather than a browser.
 *
 * Read once: the bridge is exposed by a preload that runs before any script in the
 * document, and cannot appear later — and a value that could change mid-session
 * would put the FAB and the menu bar on screen at the same time.
 */
const EXPOSED =
  typeof window === "undefined"
    ? undefined
    : (window as unknown as { remotexNative?: NativeHostBridge | null })
        .remotexNative;

// `!== null` as well as the `typeof`, which alone is true of `null`: a bridge that
// is present and empty would put this page in "the shell is listening" mode with
// nothing on the other end, and `useNativeCommands` would throw on mount.
export const NATIVE_HOST: boolean =
  typeof EXPOSED === "object" && EXPOSED !== null;

function bridge(): NativeHostBridge {
  return (window as unknown as { remotexNative: NativeHostBridge })
    .remotexNative;
}

/**
 * Send one event to the app. A no-op in a browser, so callers never branch.
 *
 * Failures are swallowed: the page is the thing on screen, and an app that has
 * stopped listening is not a reason for it to stop working.
 */
export function postToHost(event: NativeEvent): void {
  if (!NATIVE_HOST) {
    return;
  }
  try {
    bridge().post(event);
  } catch {
    // The app is gone or refused the message; the page carries on regardless.
  }
}

/**
 * Post `state` to the app whenever it changes.
 *
 * Compared as JSON rather than field by field: it is a dozen values that change
 * together, the object is rebuilt on every render, and the alternative is a
 * dependency list that goes stale the first time a field is added.
 */
export function useNativeState(state: NativeState): void {
  const encoded = JSON.stringify(state);
  useEffect(() => {
    if (!NATIVE_HOST) {
      return;
    }
    postToHost({ type: "state", state: JSON.parse(encoded) as NativeState });
  }, [encoded]);
}

/**
 * Receive commands for as long as the caller is mounted.
 *
 * The subscription is torn down on unmount, which is what stops a menu item from
 * driving a session that has already ended.
 */
export function useNativeCommands(handlers: NativeCommandHandlers): void {
  useEffect(() => {
    if (!NATIVE_HOST) {
      return;
    }
    return bridge().onCommand((command) => {
      // A command this build does not know is dropped rather than thrown: the
      // app and the page ship together, so it can only mean a hand-typed one in
      // the developer tools.
      const handler = handlers[command.type] as
        | ((c: NativeCommand) => void)
        | undefined;
      handler?.(command);
    });
  }, [handlers]);
}
