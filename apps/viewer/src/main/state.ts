// What the page reported, and everything the menu bar reads off it.
//
// The app holds no session: no claim, no socket, no wire format, no framebuffer.
// The client has all of that and posts one object saying what it has decided, and
// every title, tick and enablement below is derived from that object and from
// nothing else.
//
// The rule that keeps the two from drifting: **nothing here is ever set
// optimistically**. A tick moves when the page says the thing changed, not when the
// item was pressed.

import type { NativeState } from "../shared/contract.ts";

/**
 * What this app calls itself before a gateway has said otherwise. Mirrors
 * `DEFAULT_BRANDING` in `src/config.rs`, which is what the gateway answers with
 * when the config names nothing.
 */
export const DEFAULT_BRANDING = "remotex";

/** What the window is showing. */
export type Screen =
  /** Starting the gateway in this bundle, or explaining why it did not start. */
  | { phase: "launching" }
  /** The gateway is up and the page is showing. */
  | { phase: "ready" };

/**
 * A page that has not reported yet, and the state a launch screen shows.
 *
 * Every field has a value here rather than being optional, so a menu derived from
 * it reads "nothing is connected" instead of having to test for absence.
 */
export function blankState(): NativeState {
  return {
    branding: DEFAULT_BRANDING,
    mode: "picker",
    status: "connecting",
    ready: false,
    editing: false,
    size: null,
    canClipboard: false,
    canAudio: false,
    audioEnabled: false,
    audioError: null,
    displays: [],
    activeDisplayId: null,
    macKeyOverridesEnabled: true,
    macKeyOverridesActive: false,
    remoteIsMac: false,
  };
}

/**
 * Take one reported state, filling in anything a page mid-navigation left out.
 *
 * A partial or malformed report leaves the menus describing nothing rather than
 * describing the session before it.
 */
export function acceptState(reported: unknown): NativeState {
  const blank = blankState();
  if (typeof reported !== "object" || reported === null) {
    return blank;
  }
  const source = reported as Record<string, unknown>;
  const take = <K extends keyof NativeState>(
    key: K,
    is: (value: unknown) => boolean,
  ): NativeState[K] =>
    is(source[key]) ? (source[key] as NativeState[K]) : blank[key];

  const string = (v: unknown) => typeof v === "string";
  const bool = (v: unknown) => typeof v === "boolean";
  // Finite and positive, not merely `number`: `NaN` and `Infinity` are numbers, and
  // they reach a window resize and a menu title from here. A desktop "NaN × NaN" is
  // the readable half of that; the other half is a window asked for a size no
  // display has.
  const size = (v: unknown) =>
    typeof v === "number" && v > 0 && Number.isFinite(v);
  const display = (v: unknown) => {
    if (typeof v !== "object" || v === null) {
      return false;
    }
    const d = v as Record<string, unknown>;
    return (
      typeof d.id === "number" &&
      typeof d.label === "string" &&
      typeof d.detail === "string" &&
      typeof d.main === "boolean" &&
      typeof d.virtual === "boolean"
    );
  };

  return {
    branding: take("branding", (v) => string(v) && v !== ""),
    mode: take("mode", (v) => v === "picker" || v === "desktop"),
    status: take("status", string),
    ready: take("ready", bool),
    editing: take("editing", bool),
    size: take(
      "size",
      (v) =>
        typeof v === "object" &&
        v !== null &&
        size((v as { w?: unknown }).w) &&
        size((v as { h?: unknown }).h) &&
        size((v as { scale?: unknown }).scale),
    ),
    canClipboard: take("canClipboard", bool),
    canAudio: take("canAudio", bool),
    audioEnabled: take("audioEnabled", bool),
    audioError: take("audioError", (v) => string(v) || v === null),
    // All or nothing: a list with one malformed entry is a report to distrust, and
    // a menu that silently drops a screen is worse than one that lists none.
    displays: take("displays", (v) => Array.isArray(v) && v.every(display)),
    activeDisplayId: take(
      "activeDisplayId",
      (v) => typeof v === "number" || v === null,
    ),
    macKeyOverridesEnabled: take("macKeyOverridesEnabled", bool),
    macKeyOverridesActive: take("macKeyOverridesActive", bool),
    remoteIsMac: take("remoteIsMac", bool),
  };
}

/** The screen and the report together: everything a menu is a function of. */
export interface ViewerState {
  screen: Screen;
  state: NativeState;
}

/** Whether a session command has anything to act on. */
export function isOnDesktop({ screen, state }: ViewerState): boolean {
  return (
    screen.phase === "ready" &&
    state.mode === "desktop" &&
    state.status === "connected"
  );
}

/**
 * Whether the guest should be given every ⌘ chord, which is the shell's one input
 * decision and the reason this app exists.
 *
 * While this is true the menu bar's own shortcuts are suppressed, so ⌘W, ⌘Q and ⌘T
 * reach the page as ordinary key events. `editing` is what hands them back: with a
 * caret in a panel's text box, ⌘V has to paste into the field, and the Edit menu is
 * the only thing that can do that.
 */
export function guestOwnsKeyboard(viewer: ViewerState): boolean {
  return isOnDesktop(viewer) && viewer.state.ready && !viewer.state.editing;
}

/** The window's title. */
export function windowTitle({ state }: ViewerState): string {
  // A speaker while sound is playing: the toggle is a menu item nobody is looking
  // at, and this is the one surface that is always visible.
  return state.mode === "desktop" && state.audioEnabled
    ? `${state.branding} 🔊`
    : state.branding;
}

/**
 * Named for what it is doing rather than for what it is set to: against a Mac guest
 * there is nothing to translate, and a plain unticked box would read as a preference
 * somebody turned off.
 */
export function macKeyOverridesLabel({ state }: ViewerState): string {
  return state.remoteIsMac
    ? "Enable macOS Keyboard Overrides (Not Applicable)"
    : "Enable macOS Keyboard Overrides";
}

/**
 * Whether the override is a choice at all. Beside the label above, because the two
 * answer the same question and a menu that greys on one rule while labelling itself
 * by another is how they come to disagree.
 */
export function canOverrideMacKeys(viewer: ViewerState): boolean {
  return isOnDesktop(viewer) && !viewer.state.remoteIsMac;
}

/**
 * Fit the *window* to the remote. Nothing is sent for it, so it needs no permission
 * — only a desktop with a known size. On a `resize = true` target the remote then
 * follows the fitted window, which is exactly what fitting means there.
 */
export function canResizeToDisplay(viewer: ViewerState): boolean {
  return isOnDesktop(viewer) && viewer.state.size !== null;
}

export function canClipboard(viewer: ViewerState): boolean {
  return isOnDesktop(viewer) && viewer.state.canClipboard;
}

export function canAudio(viewer: ViewerState): boolean {
  return isOnDesktop(viewer) && viewer.state.canAudio;
}

/**
 * Whether somebody else holds the session, and which way round.
 *
 * `null` hides the item rather than greying it: "Take Over Session" on a session
 * nobody else has is not an action that is unavailable, it is one that has no
 * meaning.
 */
export function takeOverTitle({ screen, state }: ViewerState): string | null {
  if (screen.phase !== "ready") {
    return null;
  }
  switch (state.status) {
    case "busy":
      return "Take Over Session";
    case "takenOver":
      return "Take Session Back";
    default:
      return null;
  }
}

/**
 * The Display menu's read-only line.
 *
 * A readout rather than a command, and present for every target: on RDP and on
 * plain VNC that menu is otherwise empty and these numbers appear nowhere at all.
 */
export function displaySummaryLine(
  { state }: ViewerState,
  hostScale: number,
): string {
  if (!state.size) {
    return "No Remote Desktop";
  }
  const { w, h, scale } = state.size;
  const points = `${Math.round(w / scale)} × ${Math.round(h / scale)}`;
  const pixels = `${w} × ${h}`;
  const remote = scale === 1 ? `${pixels} px` : `${points} pt (${pixels} px)`;
  return `Remote ${remote} · This Display ${hostScale}×`;
}

/** The clipboard follows the pasteboard only while there is a desktop to send to. */
export function clipboardShouldFollow(viewer: ViewerState): boolean {
  return (
    viewer.screen.phase === "ready" &&
    viewer.state.mode === "desktop" &&
    viewer.state.canClipboard
  );
}
