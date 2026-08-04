// What crosses between this SPA and `remotex.app`, as types and nothing else.
//
// Apart from `nativeHost.ts` for one reason: **this file must not import React**,
// or anything that does. The shell type-checks it — `apps/viewer/src/shared/
// contract.ts` imports these three straight out of the client, so that a rename
// here is a `tsc` error there rather than a menu that silently stops working — and
// the shell has no React in its dependencies and no business having one. Its own
// check would then need the client's whole node_modules to run, which is exactly
// how it failed in CI the first time: green on a machine where `frontend/` happened
// to be installed, `Cannot find module 'react'` on one where it was not.
//
// `nativeHost.ts` re-exports all of it, so nothing in the client imports from here.

import type { DisplayInfo } from "./protocol.ts";

/// The remote framebuffer and its density, structurally the `RemoteSize` in
/// useRemoteDesktop. Declared here rather than imported because that module
/// imports this one, and a type-only cycle is still a cycle to read.
export interface HostRemoteSize {
  w: number;
  h: number;
  scale: number;
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
