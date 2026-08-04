// The menu bar, which is most of what this app is.
//
// Every item stands in for a control the client hides inside this window (see
// `chromeless` in `frontend/src/RemoteDesktop.tsx`): the same actions a browser puts
// in a floating drawer, in the place a Mac app puts them. Each one sends a command
// over the bridge, and each one's title, tick and enablement is read back off what
// the page reported — so a menu can never claim something the thing on screen is not
// doing.
//
// A plain descriptor tree rather than an Electron template, because then the whole
// menu is a pure function of one state object and can be tested as one. Installing
// it is `menu-install.ts`'s job and it is the only part that needs an app running.
//
// **No item outside the App, Edit and Window menus carries a key equivalent.** While
// a desktop is focused every ⌘ chord belongs to the guest, so an advertised shortcut
// would be one that does something else.

import type { NativeCommand } from "../shared/contract.ts";
import {
  autoResizeLabel,
  canAudio,
  canAutoResize,
  canClipboard,
  canOverrideMacKeys,
  canResizeNow,
  canResizeToDisplay,
  displaySummaryLine,
  isOnDesktop,
  macKeyOverridesLabel,
  takeOverTitle,
  type ViewerState,
} from "./state.ts";

/** Things the shell does itself, with no command to send. */
export type LocalAction =
  | "about"
  | "quit"
  | "configure"
  | "restartGateway"
  | "devTools"
  | "resizeToDisplay";

export interface MenuItemSpec {
  label: string;
  enabled?: boolean;
  checked?: boolean;
  /** A separator, and the only item that carries nothing else. */
  separator?: boolean;
  /** A standard Electron role — the Edit and Window menus, and nothing else. */
  role?: string;
  command?: NativeCommand;
  action?: LocalAction;
  submenu?: MenuItemSpec[];
}

export interface MenuSpec {
  label: string;
  items: MenuItemSpec[];
}

const SEPARATOR: MenuItemSpec = { label: "-", separator: true };

/**
 * The chords a guest needs and this keyboard cannot type.
 *
 * Two kinds, and they are different things. The first are chords macOS keeps for
 * itself or spells differently — ⌥F4 has no Mac equivalent at all. The second are
 * single modifier *taps*, which a guest reads as "open the menu" and which cannot be
 * typed here: holding a modifier and letting go is, as far as this Mac's keyboard is
 * concerned, a chord with nothing in it.
 */
export const SPECIAL_CHORDS: { label: string; codes: string[] }[] = [
  { label: "F5", codes: ["F5"] },
  { label: "F11", codes: ["F11"] },
  { label: "Ctrl+R", codes: ["ControlLeft", "KeyR"] },
  { label: "Ctrl+W", codes: ["ControlLeft", "KeyW"] },
  { label: "Ctrl+T", codes: ["ControlLeft", "KeyT"] },
  { label: "Alt+F4", codes: ["AltLeft", "F4"] },
];

export const MODIFIER_TAPS: { label: string; codes: string[] }[] = [
  { label: "Ctrl", codes: ["ControlLeft"] },
  { label: "Alt", codes: ["AltLeft"] },
  { label: "Shift", codes: ["ShiftLeft"] },
  { label: "Super", codes: ["MetaLeft"] },
];

export interface MenuContext {
  viewer: ViewerState;
  /** This window's backing scale, for the Display menu's readout. */
  hostScale: number;
  /** Whether there is a gateway to restart and a config to edit. */
  hasGateway: boolean;
  /** False while a restart is already under way. */
  idle: boolean;
}

/** The whole menu bar, derived. */
export function buildMenuSpec(context: MenuContext): MenuSpec[] {
  return [
    appMenu(context),
    editMenu(),
    remoteMenu(context),
    displayMenu(context),
    viewMenu(context),
    windowMenu(),
  ];
}

function appMenu({ viewer }: MenuContext): MenuSpec {
  return {
    label: viewer.state.branding,
    items: [
      // Named after the instance's branding, which is what the rest of its windows
      // say — two instances running at once are told apart by it.
      { label: `About ${viewer.state.branding}`, action: "about" },
      SEPARATOR,
      { label: "Services", role: "services" },
      SEPARATOR,
      { label: "Hide", role: "hide" },
      { label: "Hide Others", role: "hideOthers" },
      { label: "Show All", role: "unhide" },
      SEPARATOR,
      // ⌘Q keeps its accelerator, and keeps working: while a live desktop has focus
      // the shell suppresses it and the guest gets the chord, and anywhere else —
      // the picker, a panel's text box, the launch screen — it quits, as it should.
      { label: `Quit ${viewer.state.branding}`, action: "quit", role: "quit" },
    ],
  };
}

/**
 * The standard Edit menu, with its standard accelerators.
 *
 * Required rather than decorative: without it ⌘C and ⌘V do nothing in a text field
 * on macOS, and the clipboard panel and the configuration editor are both text
 * fields.
 */
function editMenu(): MenuSpec {
  return {
    label: "Edit",
    items: [
      { label: "Undo", role: "undo" },
      { label: "Redo", role: "redo" },
      SEPARATOR,
      { label: "Cut", role: "cut" },
      { label: "Copy", role: "copy" },
      { label: "Paste", role: "paste" },
      { label: "Select All", role: "selectAll" },
    ],
  };
}

function remoteMenu(context: MenuContext): MenuSpec {
  const { viewer, hasGateway, idle } = context;
  const onDesktop = isOnDesktop(viewer);
  const items: MenuItemSpec[] = [
    {
      label: macKeyOverridesLabel(viewer),
      enabled: canOverrideMacKeys(viewer),
      checked: viewer.state.macKeyOverridesActive,
      command: {
        type: "setMacKeyOverrides",
        enabled: !viewer.state.macKeyOverridesActive,
      },
    },
    SEPARATOR,
    // Opens the client's own clipboard panel, which fetches the remote's text first
    // and shows it concealed. Rebuilding that natively would be two clipboard
    // editors to keep in step, and the consent boundary — the panel's Copy button —
    // would then exist in two places.
    {
      label: "Clipboard…",
      enabled: canClipboard(viewer),
      command: { type: "openClipboard" },
    },
    // Sound from the remote. Greyed rather than hidden for a target that has none: a
    // menu whose items come and go is harder to learn than one that is sometimes
    // disabled. It says nothing about whether sound is *arriving*, because from this
    // end a quiet remote and one that will never redirect are the same thing.
    {
      label: "Enable Audio",
      enabled: canAudio(viewer),
      checked: viewer.state.audioEnabled,
      command: { type: "setAudio", enabled: !viewer.state.audioEnabled },
    },
    // The escape hatch for a framebuffer that has gone wrong: re-announce the size
    // and repaint everything.
    { label: "Refresh", enabled: onDesktop, command: { type: "refresh" } },
    {
      label: "Switch Target",
      enabled: onDesktop,
      command: { type: "switchTarget" },
    },
    {
      label: "Send Keys",
      enabled: onDesktop,
      submenu: [
        ...SPECIAL_CHORDS.map(({ label, codes }) => ({
          label,
          command: { type: "sendKeyCombo" as const, codes },
        })),
        SEPARATOR,
        ...MODIFIER_TAPS.map(({ label, codes }) => ({
          label: `Tap ${label}`,
          command: { type: "sendKeyCombo" as const, codes },
        })),
      ],
    },
  ];

  const takeOver = takeOverTitle(viewer);
  if (takeOver) {
    items.push({ label: takeOver, command: { type: "takeOver" } });
  }

  items.push(
    SEPARATOR,
    // Where the targets come from. Reachable from every screen, including the launch
    // failure — a config the gateway refused is the likeliest reason to be looking
    // for this item at all.
    { label: "Configuration…", enabled: hasGateway, action: "configure" },
    // Everything below the app is rebuilt from the config on disk, the token
    // included, which is why the page is reloaded rather than left holding the old
    // one.
    {
      label: "Restart Local Gateway",
      enabled: hasGateway && idle,
      action: "restartGateway",
    },
    SEPARATOR,
    // The inspector, on the client, in the shipped app rather than a debug build:
    // the shipped app is the one there is to look at, and the alternative to reading
    // a console error is guessing at it from the outside.
    {
      label: "Developer Tools",
      enabled: viewer.screen.phase === "ready",
      action: "devTools",
    },
  );

  return { label: "Remote", items };
}

function displayMenu(context: MenuContext): MenuSpec {
  const { viewer } = context;
  const items: MenuItemSpec[] = [
    {
      label: displaySummaryLine(viewer, context.hostScale),
      enabled: false,
    },
    SEPARATOR,
  ];
  const displays = isOnDesktop(viewer) ? viewer.state.displays : [];
  if (displays.length === 0) {
    items.push({ label: "No Displays to Choose From", enabled: false });
  } else {
    for (const display of displays) {
      items.push({
        label: `${display.label} — ${display.detail}`,
        checked: display.id === viewer.state.activeDisplayId,
        // Only ever selected, never cleared: the remote is always sharing exactly
        // one display, so there is no "off" to honour.
        command: { type: "selectDisplay", id: display.id },
      });
    }
  }
  return { label: "Display", items };
}

function viewMenu(context: MenuContext): MenuSpec {
  const { viewer } = context;
  return {
    label: "View",
    items: [
      {
        label: autoResizeLabel(viewer),
        enabled: canAutoResize(viewer),
        checked: viewer.state.autoResize,
        command: {
          type: "setAutoResize",
          enabled: !viewer.state.autoResize,
        },
      },
      {
        label: "Resize to Window",
        enabled: canResizeNow(viewer),
        command: { type: "resizeToWindow" },
      },
      {
        label: "Resize to Display",
        enabled: canResizeToDisplay(viewer),
        action: "resizeToDisplay",
      },
      SEPARATOR,
      { label: "Toggle Full Screen", role: "togglefullscreen" },
    ],
  };
}

/**
 * The standard Window menu, with one item missing on purpose: **Close**.
 *
 * ⌘W is one of the two chords this whole app exists to hand to the guest, and an
 * item carrying it would take it back the moment the desktop lost focus for a
 * fraction of a second. There is one window and quitting closes it.
 */
function windowMenu(): MenuSpec {
  return {
    label: "Window",
    items: [
      { label: "Minimize", role: "minimize" },
      { label: "Zoom", role: "zoom" },
      SEPARATOR,
      { label: "Bring All to Front", role: "front" },
    ],
  };
}

/** Every item in the tree, for tests that ask a question of all of them. */
export function everyItem(menus: MenuSpec[]): MenuItemSpec[] {
  const flat: MenuItemSpec[] = [];
  const walk = (items: MenuItemSpec[]) => {
    for (const item of items) {
      flat.push(item);
      if (item.submenu) {
        walk(item.submenu);
      }
    }
  };
  for (const menu of menus) {
    walk(menu.items);
  }
  return flat;
}
