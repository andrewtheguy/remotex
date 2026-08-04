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
import type { DisplayInfo } from "./protocol.ts";

/// The remote framebuffer and its density, structurally the `RemoteSize` in
/// useRemoteDesktop. Declared here rather than imported because that module
/// imports this one, and a type-only cycle is still a cycle to read.
interface HostRemoteSize {
  w: number;
  h: number;
  scale: number;
}

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
export const NATIVE_HOST: boolean =
  typeof window !== "undefined" &&
  typeof (window as unknown as { remotexNative?: NativeHostBridge })
    .remotexNative === "object";

function bridge(): NativeHostBridge {
  return (window as unknown as { remotexNative: NativeHostBridge })
    .remotexNative;
}

/** Everything the menu bar needs to render itself. Posted whenever it changes. */
export interface NativeState {
  /**
   * This gateway's display name, from `GET /api/config`.
   *
   * Reported rather than fetched by the app, because the app makes no requests:
   * the page is the only thing here that talks to the gateway. It names the
   * window, the About item and the About panel, all of which are the shell's.
   */
  branding: string;
  /** Which screen the client is on: the menus are dead outside the desktop. */
  mode: "picker" | "desktop";
  /** The connection lifecycle, which decides Take Over's title and presence. */
  status: string;
  /** True once the first frame has arrived, which is when input may be captured. */
  ready: boolean;
  /**
   * Whether the caret is in something typeable — a panel's text box, not the
   * remote surface.
   *
   * The shell's one input decision hangs off this. While a live desktop has focus
   * it drops its menu accelerators so every ⌘ chord reaches the guest; the moment
   * this turns true it hands them back, so ⌘V pastes into the clipboard editor
   * instead of being forwarded to a remote that cannot see the field.
   */
  editing: boolean;
  /** The remote framebuffer and its density, for **Resize to Display**. */
  size: HostRemoteSize | null;
  /** The two resize permissions and the client's per-session choice. */
  canResize: boolean;
  canAutoResize: boolean;
  autoResize: boolean;
  canClipboard: boolean;
  canAudio: boolean;
  audioEnabled: boolean;
  audioError: string | null;
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  /** The Command-translation preference and whether it is doing anything. */
  macKeyOverridesEnabled: boolean;
  macKeyOverridesActive: boolean;
  remoteIsMac: boolean;
}

/** What the page tells the app. */
export type NativeEvent =
  | { type: "state"; state: NativeState }
  /**
   * The remote's clipboard changed on its own. The app writes the system
   * pasteboard; this is the one direction a page cannot take itself, since
   * writing the pasteboard from a document needs a gesture it does not have.
   */
  | { type: "clipboardFromRemote"; text: string }
  /**
   * `/api/auth/status` said no, so the token the app put in the cookie store is
   * not the one this gateway minted. A login form cannot fix that, so the app
   * shows its own failure screen instead of letting one appear.
   */
  | { type: "unauthenticated" };

/** What the app tells the page. */
export type NativeCommand =
  /** The Mac's pasteboard changed; forward it to the remote. */
  | { type: "clipboardLocal"; text: string }
  /** Menu commands, each standing in for a control the shell hides. */
  | { type: "openClipboard" }
  | { type: "openDisplays" }
  | { type: "closePanel" }
  | { type: "resizeToWindow" }
  | { type: "setAutoResize"; enabled: boolean }
  | { type: "selectDisplay"; id: number }
  | { type: "setAudio"; enabled: boolean }
  | { type: "setMacKeyOverrides"; enabled: boolean }
  | { type: "refresh" }
  | { type: "switchTarget" }
  | { type: "takeOver" }
  /**
   * A chord no browser and no menu bar will let through — ⌥F4, a bare modifier
   * tap. **Send Keys ▸** is the one keyboard item on the menu, and it exists for
   * the keys macOS itself keeps rather than for the ones the shell hands over.
   */
  | { type: "sendKeyCombo"; codes: string[] };

/** Handlers for every command the app can send. */
export type NativeCommandHandlers = {
  [C in NativeCommand as C["type"]]: (command: C) => void;
};

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
