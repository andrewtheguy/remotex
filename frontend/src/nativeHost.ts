// The seam between this SPA and `remotex.app`, which shows it in an embedded
// Chromium.
//
// The app is a *shell*: it owns the window, the menu bar, the macOS pasteboard and
// keyboard capture, and nothing else. Everything below the title bar is this client,
// the same build a browser loads, talking to the same gateway over the same socket.
// So the seam carries only what a page cannot do for itself:
//
//   app → page   keys the browser is never given (⌘W, ⌘Q, ⌘T…), the local
//                pasteboard, and the menu commands that stand in for the floating
//                menu the shell hides;
//   page → app   one state object the menus derive their titles, ticks and
//                enablement from, plus the remote's clipboard when it changes.
//
// Nothing about the session crosses it. The app holds no claim, no socket and no
// wire format — which is the point: a protocol change is a change to this client
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
 * The query function `remotex.app`'s embedded Chromium installs on `window`.
 *
 * One string in, nothing back. The app never answers — every command it has for
 * the page goes the other way, through {@link useNativeCommands} — so the
 * `onSuccess` and `onFailure` members the message router also accepts are left
 * off, and this is a one-way post shaped like a query.
 */
type NativeQuery = (query: { request: string }) => number;

/**
 * Whether this page is running inside `remotex.app` rather than a browser.
 *
 * Read once: the function is installed as each frame's V8 context is created,
 * before any script in it runs, and cannot appear later — and a value that could
 * change mid-session would put the FAB and the menu bar on screen at the same time.
 */
export const NATIVE_HOST: boolean =
  typeof window !== "undefined" &&
  typeof (window as unknown as { remotexNative?: NativeQuery })
    .remotexNative === "function";

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
   * The remote's clipboard changed on its own. The app writes `NSPasteboard`;
   * this is the one direction a web view cannot take itself, since writing the
   * system pasteboard from a page needs a gesture it does not have.
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
  /** One key, already mapped from a macOS virtual keycode to a DOM `code`. */
  | {
      type: "key";
      code: string;
      pressed: boolean;
      caps: boolean;
      meta: boolean;
    }
  /** Let go of everything held — focus left the surface, or capture stopped. */
  | { type: "releaseInput" }
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
    // Serialized here rather than handed over as an object: the message router
    // carries a string, and doing it on this side means the app decodes exactly
    // what this file's types describe.
    (window as unknown as { remotexNative: NativeQuery }).remotexNative({
      request: JSON.stringify(event),
    });
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

/** The property the app calls into. Global because executing JavaScript in a frame
 * has no other way in — it evaluates a string against the page's global object. */
const COMMAND_PROPERTY = "__remotexNative";

interface CommandTarget {
  command: (command: NativeCommand) => void;
}

/**
 * Install the command entry point for as long as the caller is mounted.
 *
 * Handlers are read through a ref-like closure over the latest object, because the
 * app may call in at any moment and re-installing the global on every render would
 * lose a command that arrived mid-swap.
 */
export function useNativeCommands(handlers: NativeCommandHandlers): void {
  useEffect(() => {
    if (!NATIVE_HOST) {
      return;
    }
    const target = window as unknown as Record<string, CommandTarget>;
    target[COMMAND_PROPERTY] = {
      command: (command: NativeCommand) => {
        // A command this build does not know is dropped rather than thrown: the
        // app and the page ship together, so it can only mean a hand-typed one in
        // the Web Inspector.
        const handler = handlers[command.type] as
          | ((c: NativeCommand) => void)
          | undefined;
        handler?.(command);
      },
    };
    return () => {
      delete target[COMMAND_PROPERTY];
    };
  }, [handlers]);
}
