import {
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { appWindow } from "./appWindow.ts";
import { ClipboardPanel } from "./ClipboardPanel.tsx";
import DisplayPanel from "./DisplayPanel.tsx";
import {
  enterImmersive,
  exitImmersive,
  available as immersiveAvailable,
  keyboardLockHeld,
  onKeyboardLockChange,
} from "./immersive.ts";
import type {
  ClipboardSnapshot,
  DisplayInfo,
  RemoteClipboard,
} from "./protocol.ts";
import { SoftKeyboardPanel } from "./SoftKeyboardPanel.tsx";
import { densityLabel, type RemoteSize } from "./useRemoteDesktop.ts";

// The floating chrome — a draggable ☰ button that toggles a toolbar drawer. The
// drawer carries this project's controls (browser-swallowed keys, modifier taps,
// the gesture cheat-sheet), a Switch target button that returns to the post-login
// picker, and Log out, which ends the web login. Three of its buttons open a panel
// instead of acting: Soft keyboard, Clipboard — only for targets that opted into
// it, see ClipboardPanel — and Display, only for a remote that offers more than
// one (see DisplayPanel).
const FAB_SIZE = 40;
const FAB_MARGIN = 12;
// Pointer travel (px) before a press becomes a drag rather than a click.
const DRAG_THRESHOLD = 6;
const TOOLBAR_WIDTH = 240;
const TOOLBAR_GAP = 10;
const TOOLBAR_MIN_HEIGHT = 120;

// DOM `code` sequences the browser intercepts before the remote can see them:
// pressed in order, released in reverse (see useRemoteDesktop.sendKeyCombo).
const SPECIAL_KEYS: readonly { label: string; codes: string[] }[] = [
  { label: "F5", codes: ["F5"] },
  { label: "F11", codes: ["F11"] },
  { label: "Ctrl+R", codes: ["ControlLeft", "KeyR"] },
  { label: "Ctrl+W", codes: ["ControlLeft", "KeyW"] },
  { label: "Ctrl+T", codes: ["ControlLeft", "KeyT"] },
  { label: "Alt+F4", codes: ["AltLeft", "F4"] },
];

// Bare modifier taps — useful on touch, where there's no physical modifier to
// hold while tapping another key.
const MODIFIER_TAPS: readonly { label: string; code: string }[] = [
  { label: "Ctrl", code: "ControlLeft" },
  { label: "Alt", code: "AltLeft" },
  { label: "Shift", code: "ShiftLeft" },
  { label: "Super", code: "MetaLeft" },
];

// The chrome shortcut, spelled the way the Help card and the button's tooltip show
// it. One source for all three so the card, the tooltip and the handler cannot
// drift — the handler matches exactly what this returns.
//
// The middle modifier is the host's: Command on a Mac, where Option is a
// character-composing key and Ctrl+Alt chords are Windows vocabulary, and Alt
// everywhere else. Whichever it is, the *other* one has to be absent, so all four
// held — a Mac user's own Cmd+Ctrl+Alt+Shift+; hyper chord — stays theirs.
function hideChromeShortcut(isMacHost: boolean): string {
  return isMacHost ? "Ctrl + Cmd + Shift + ;" : "Ctrl + Alt + Shift + ;";
}

// The reference density both ends of this agree on: one CSS pixel per dot, and
// also what RDP calls 100%. So a 2x screen is 192 dpi whichever end names it.
const CSS_DPI = 96;

// A density in the unit a host's own display settings talk in, since that is where
// someone checks whether a remote actually applied one.
function dpiLabel(hundredths: number): string {
  return `${Math.round((hundredths / 100) * CSS_DPI)} dpi`;
}

// What the Mac key override does, which is more than fits on the button that
// toggles it — hence "Mac key override: on" there and the detail here. Shown only
// on a Mac host, the only place that button exists. Mirrors macKeys.ts, including
// the six chords a browser keeps whatever this is set to.
const MAC_KEY_HELP: readonly { situation: string; effect: string }[] = [
  {
    situation: "While on",
    effect: "⌘A ⌘C ⌘F ⌘P ⌘S ⌘V ⌘X ⌘Z arrive as Ctrl chords",
  },
  { situation: "While off", effect: "Every key arrives as pressed" },
  { situation: "⌘ on its own", effect: "Arrives as the Windows key" },
  { situation: "Kept by this browser", effect: "⌘W ⌘T ⌘N ⌘L ⌘O ⌘R" },
  { situation: "A Mac remote", effect: "n/a — ⌘ is sent as ⌘" },
];

// The touch gesture cheat-sheet, mirroring touchGestures.ts.
const GESTURE_HELP: readonly { gesture: string; action: string }[] = [
  { gesture: "Tap", action: "Left-click" },
  { gesture: "Double-tap and hold", action: "Grab, then drag" },
  { gesture: "One-finger drag", action: "Move cursor + pan" },
  { gesture: "Two-finger tap", action: "Right-click" },
  { gesture: "Two-finger pinch", action: "Zoom" },
  { gesture: "Three-finger swipe", action: "Scroll" },
];

interface Position {
  x: number;
  y: number;
}

interface DragState {
  pointerId: number;
  startX: number;
  startY: number;
  originX: number;
  originY: number;
  dragged: boolean;
}

// visualViewport tracks the *visible* area (mobile URL bar, on-screen keyboard,
// pinch-zoom), with a window fallback for browsers that lack it.
interface Viewport {
  width: number;
  height: number;
  offsetX: number;
  offsetY: number;
}

function readViewport(): Viewport {
  const vp = window.visualViewport;
  return {
    width: vp ? vp.width : window.innerWidth,
    height: vp ? vp.height : window.innerHeight,
    offsetX: vp ? vp.offsetLeft : 0,
    offsetY: vp ? vp.offsetTop : 0,
  };
}

// Which docked panel is open, if any.
//
// One state rather than a boolean each: both dock to the bottom edge and report
// the same canvas inset, so a second one open would sit on the first. Two
// booleans made that a rule every call site had to remember — open this one,
// clear the other — and this makes it impossible to express.
type Panel = "clipboard" | "keyboard" | "display";

/// The panel actions, for a host with its own menus. The clipboard one fetches
/// first and resolves when the panel is up, exactly as the drawer's button does —
/// there is one implementation of "open the clipboard", and a menu item calls it.
export interface PanelControls {
  openClipboard: () => void;
  toggleDisplays: () => void;
  closePanel: () => void;
  /**
   * Show the Help card, which is where "This session" lives.
   *
   * The one route to it in a chromeless host: the ☰ button that opens it is the
   * chrome such a host replaces, so without this the card — the remote's size and
   * density against this window's, the connection, the render dial — is
   * unreachable in `remotex.app`. Reported by the page rather than
   * rebuilt natively, because every line of it is derived from state this page
   * already holds and a second copy is a second copy to keep in step.
   */
  openHelp: () => void;
}

// What Immersive is *for*, which is not the same in both window kinds: a tab is buying
// the six chords a browser keeps, and an app window — which reserves none of them
// already — is buying screen area. Saying the second thing in the first window's words
// would be promising something that is already true.
const IMMERSIVE_EXIT_HINT = appWindow()
  ? "Leave full screen"
  : "Leave full screen and give this browser its shortcuts back";

const IMMERSIVE_ENTER_HINT = appWindow()
  ? "Full screen. This window already sends ⌘W, ⌘T and the rest to the remote"
  : "Full screen, and send ⌘W, ⌘T and the rest to the remote instead of this browser";

/// Full screen plus a keyboard lock, which is how a browser *tab* is given ⌘W, ⌘T and
/// the four other chords it otherwise keeps for itself. See immersive.ts.
///
/// Absent, not disabled, where the browser has no lock to give: a greyed button here
/// would be explaining an API Firefox and Safari do not have, which is not this menu's
/// job. It is its own component because the lock is a browser-wide condition rather
/// than this menu's state — a held Esc or the platform's own ⌃⌘F ends one without
/// anything here being clicked — so the subscription belongs next to the label it
/// keeps honest.
function ImmersiveButton({ onToggle }: { onToggle: () => void }) {
  const locked = useSyncExternalStore(
    onKeyboardLockChange,
    keyboardLockHeld,
    () => false,
  );
  if (!immersiveAvailable()) {
    return null;
  }
  return (
    <button
      type="button"
      className="toolbar-btn"
      aria-pressed={locked}
      onClick={() => {
        onToggle();
        if (locked) {
          void exitImmersive();
        } else {
          void enterImmersive();
        }
      }}
      title={locked ? IMMERSIVE_EXIT_HINT : IMMERSIVE_ENTER_HINT}
    >
      {locked ? "Exit immersive" : "Immersive"}
    </button>
  );
}

/// The one thing about a keyboard lock a user has to be told: the way out is the
/// browser's own, it cannot be captured by anything on this page or on any remote, and
/// it is what makes a locked session never a trapped one.
function ImmersiveHelpRow() {
  if (!immersiveAvailable()) {
    return null;
  }
  return (
    <div className="help-item">
      <dt>Leave immersive mode</dt>
      <dd>Hold Esc for a second — always, and uncapturable</dd>
    </div>
  );
}

/// The recommendation, shown only to the window that is not taking it.
///
/// A tab is the one configuration where the browser keeps chords back from the remote,
/// and the fix is a menu item rather than anything this client can do — so saying so is
/// the whole of what it can offer. It is also the only place the companion extension
/// runs (see docs/companion-extension.md), which is the second reason a tab is worth
/// this line and an app window is worth none.
function AppWindowHelpRow() {
  if (appWindow()) {
    return null;
  }
  return (
    <div className="help-item">
      <dt>Give this window every shortcut</dt>
      <dd>
        Chrome menu → Install page as app. ⌘W, Ctrl+W and ⌘T then reach the
        remote
      </dd>
    </div>
  );
}

function usePanel() {
  const [panel, setPanel] = useState<Panel | null>(null);
  const closePanel = useCallback(() => setPanel(null), []);
  const togglePanel = useCallback(
    (next: Panel) => setPanel((prev) => (prev === next ? null : next)),
    [],
  );
  return { panel, setPanel, closePanel, togglePanel };
}

// Whichever docked panel is open, or nothing. Rendering both from one place is
// what makes the shared inset channel safe: exactly one of them is mounted, so
// exactly one is reporting a height.
function DockedPanel({
  panel,
  onClose,
  onDockedHeightChange,
  sendKeyCombo,
  remoteClipboard,
  onSendClipboard,
  displays,
  activeDisplayId,
  onSelectDisplay,
}: {
  panel: Panel | null;
  onClose: () => void;
  onDockedHeightChange: (px: number) => void;
  sendKeyCombo: (codes: string[]) => void;
  remoteClipboard: RemoteClipboard | null;
  onSendClipboard: (text: string) => void;
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  onSelectDisplay: (id: number) => void;
}) {
  switch (panel) {
    case "keyboard":
      return (
        <SoftKeyboardPanel
          sendKeyCombo={sendKeyCombo}
          onClose={onClose}
          onDockedHeightChange={onDockedHeightChange}
        />
      );
    case "clipboard":
      return (
        <ClipboardPanel
          onSend={onSendClipboard}
          remoteClipboard={remoteClipboard}
          onClose={onClose}
          onDockedHeightChange={onDockedHeightChange}
        />
      );
    case "display":
      return (
        <DisplayPanel
          displays={displays}
          activeId={activeDisplayId}
          onSelect={onSelectDisplay}
          onClose={onClose}
          onDockedHeightChange={onDockedHeightChange}
        />
      );
    default:
      return null;
  }
}

// The drawer's Display row, naming the screen currently being shared.
//
// Absent rather than disabled when there is no choice to make, which is the
// opposite of the Clipboard row beside it — and the difference is real. A
// target without the clipboard bridge has a feature that was switched off, and
// a greyed button saying so is the answer. A target with one screen has no
// display feature at all, and a permanently greyed "Display" would be an
// explanation of nothing.
function DisplaySection({
  displays,
  activeDisplayId,
  open,
  onToggle,
}: {
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  open: boolean;
  onToggle: () => void;
}) {
  if (displays.length <= 1) {
    return null;
  }
  // Undefined for the moment between a switch and the remote's answer, and for
  // a screen unplugged out from under the session.
  const active = displays.find((display) => display.id === activeDisplayId);
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Display</span>
      {/* Unlike Clipboard, this opens straight away: the list is pushed, so
          there is nothing to fetch and nothing to wait for. */}
      <button
        type="button"
        className="toolbar-btn"
        onClick={onToggle}
        aria-pressed={open}
        title="Choose which of the remote's displays to view"
      >
        {open ? "Hide displays" : (active?.label ?? "Display")}
      </button>
    </div>
  );
}

// What the remote is drawing against what this browser is, at the top of the Help
// card so the two can be read off one another.
//
// It exists because a density that did not take is otherwise invisible. Both
// engines that match a client's density report the result only as a `resize`, and
// a request the remote quietly dropped produces no message at all: the desktop
// simply looks soft, or half the size it was asked for, with nothing saying which
// end disagreed. Two densities that ought to agree and don't is the whole
// diagnostic, which is why this reports both and not just the resolution.
//
// Shown for every target, not only the ones with a display to switch between: on
// RDP and on VNC the Display section is absent and these numbers appear nowhere
// else.
function ScreenHelp({
  size,
  hostScale,
  connection,
  renderPlan,
}: {
  size: RemoteSize | null;
  hostScale: number;
  connection: string;
  renderPlan: string;
}) {
  return (
    <>
      <h3>This session</h3>
      <dl className="help-list">
        <div className="help-item">
          <dt>Remote desktop</dt>
          {/* Null before the first `resize`, which is the "waiting for the remote
              desktop" state: a placeholder reading 0×0 would be a worse answer
              than saying so. */}
          <dd>
            {size
              ? `${size.w}×${size.h} at ${densityLabel(size.scale * 100)} (${dpiLabel(size.scale * 100)})`
              : "Waiting for the remote desktop"}
          </dd>
        </div>
        <div className="help-item">
          <dt>This browser</dt>
          <dd>
            {densityLabel(hostScale)} ({dpiLabel(hostScale)})
          </dd>
        </div>
        <div className="help-item">
          <dt>Connection</dt>
          {/* Which of the three `vnc` targets this is, where it is one of them.
              Nothing else on screen distinguishes a plain VNC server from a Mac in
              either Screen Sharing mode, and what a person notices — a display
              list, whether the desktop follows the window, a path with no
              specification behind it — follows from exactly that. Empty only
              before `connected`. */}
          <dd>{connection || "Waiting for the target"}</dd>
        </div>
        <div className="help-item">
          <dt>Render</dt>
          {/* The dial the gateway resolved, which is the one property of a session that
              decides how the picture looks and costs and that nothing else reveals: it
              lives in the operator's config file, which whoever is looking at the screen
              usually does not have. Empty only before `connected`. */}
          <dd>{renderPlan || "Waiting for the target"}</dd>
        </div>
      </dl>
    </>
  );
}

// The direct audio toggle is also the user gesture required to create a
// playable AudioContext. Targets without audio omit the row.
function AudioSection({
  available,
  enabled,
  error,
  onChange,
}: {
  available: boolean;
  enabled: boolean;
  error: string | null;
  onChange: (enabled: boolean) => void;
}) {
  if (!available) {
    return null;
  }
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Audio</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={() => onChange(!enabled)}
        aria-pressed={enabled}
        title="Play the remote's sound in this browser"
      >
        {enabled ? "Disable audio" : "Enable audio"}
      </button>
      {/* Quiet remotes have no distinct client-visible state. */}
      {error && <p className="audio-note">{error}</p>}
    </div>
  );
}

// macOS-only Command-to-Control preference. It remains visible but inactive for
// a Mac guest, where Command already has native meaning.
function MacKeyboardSection({
  enabled,
  active,
  isMacHost,
  remoteIsMac,
  onChange,
}: {
  enabled: boolean;
  active: boolean;
  isMacHost: boolean;
  remoteIsMac: boolean;
  onChange: (enabled: boolean) => void;
}) {
  if (!isMacHost) {
    return null;
  }
  // All three states named the same way, with the Help card carrying what the
  // chord table used to try to say in a button's width. "n/a" rather than "off"
  // for a Mac guest: the preference may well be on, and it is the guest that
  // makes it inapplicable.
  const label = remoteIsMac
    ? "Mac key override: n/a"
    : active
      ? "Mac key override: on"
      : "Mac key override: off";
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Mac keyboard</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={() => onChange(!enabled)}
        aria-pressed={active}
        disabled={remoteIsMac}
        title={
          remoteIsMac
            ? "This remote is a Mac, so Command chords are sent as Command"
            : "Send ⌘A ⌘C ⌘F ⌘P ⌘S ⌘V ⌘X ⌘Z to the remote as Ctrl chords, and a bare ⌘ as the Windows key. Your browser keeps ⌘W, ⌘T, ⌘N, ⌘L, ⌘O and ⌘R for itself."
        }
      >
        {label}
      </button>
    </div>
  );
}

export default function FloatingMenu({
  onLogout,
  onSwitchTarget,
  sendKeyCombo,
  onKeyboardInset,
  canClipboard,
  remoteClipboard,
  onFetchClipboard,
  onSendClipboard,
  displays,
  activeDisplayId,
  onSelectDisplay,
  size,
  hostScale,
  connection,
  renderPlan,
  canAudio,
  audioEnabled,
  audioError,
  onAudioChange,
  macKeyOverridesEnabled,
  macKeyOverridesActive,
  isMacHost,
  remoteIsMac,
  onMacKeyOverridesChange,
  onLocalShortcut,
  chromeless = false,
  onPanelControls,
}: {
  onLogout: () => void;
  // Return to the post-login target picker ("switch target"): disconnects the
  // current session without ending the login. See useRemoteDesktop.
  onSwitchTarget: () => void;
  sendKeyCombo: (codes: string[]) => void;
  // Reports the open docked panel's height so the touch canvas can inset above
  // it (0 when the panel closes or floats). See useRemoteDesktop. Both panels
  // share this channel, which is safe because only one is ever open.
  onKeyboardInset: (px: number) => void;
  // Whether the connected target opted into the clipboard bridge
  // (`clipboard = true`). False leaves the Clipboard button disabled.
  canClipboard: boolean;
  // The last clipboard reply from the server, and the fetch actions. See
  // ClipboardPanel — the browser holds no clipboard state of its own.
  // `onFetchClipboard` resolves with the remote snapshot, or null if nothing
  // answered.
  remoteClipboard: RemoteClipboard | null;
  onFetchClipboard: () => Promise<ClipboardSnapshot | null>;
  onSendClipboard: (text: string) => void;
  // The remote's displays and the one it is sharing. Empty for every engine
  // that cannot offer a choice, which is what hides the section — a list of one
  // hides it too, since there would be nothing to switch to. See DisplayPanel.
  displays: DisplayInfo[];
  activeDisplayId: number | null;
  onSelectDisplay: (id: number) => void;
  // The remote's framebuffer and its density, and this screen's density — the
  // read-only Screen section, shown for every target. See ScreenHelp for why
  // both densities and not just the size.
  size: RemoteSize | null;
  hostScale: number;
  // What this session is speaking, one line, from `connected` — the protocol and
  // the target's subtype where it has one. See connectionLabel.ts.
  connection: string;
  // The render dial this session resolved to, one line, from `connected`.
  renderPlan: string;
  // Whether this session can carry the remote's sound, which hides the Audio
  // section rather than disabling it — the same rule the Display section follows
  // and the opposite of Clipboard's. A greyed "Audio" would be explaining a
  // feature that does not exist for this target: audio is RDP-only, so on VNC
  // there is nothing that could be switched on.
  //
  // `audioEnabled` is what this browser has asked for, not proof that sound is
  // arriving: a quiet remote and one that will never redirect are the same thing
  // from the gateway's end. `audioError` is the one thing worth reporting — a
  // browser that cannot decode Opus. See useRemoteDesktop and audioPlayer.ts.
  canAudio: boolean;
  audioEnabled: boolean;
  audioError: string | null;
  onAudioChange: (enabled: boolean) => void;
  // The Command-to-Control preference and whether it is doing anything. The two
  // differ when the guest is itself a Mac, which is why the section reports the
  // reason rather than just showing the switch off. The whole section is absent
  // on a non-Mac host, where there is no Command key to translate. See macKeys.ts.
  macKeyOverridesEnabled: boolean;
  macKeyOverridesActive: boolean;
  isMacHost: boolean;
  remoteIsMac: boolean;
  onMacKeyOverridesChange: (enabled: boolean) => void;
  // A chord this component took for itself, announced to the input path so it can
  // unwind what it was holding for one. Only the Mac spelling of the chrome
  // shortcut needs it, and only because Command is in it. See useRemoteDesktop.
  onLocalShortcut: () => void;
  // Drop this component's own chrome — the button, the drawer, the help card and
  // the chord that hides them — and keep the docked panels.
  //
  // For `remotex.app`, where a real menu bar offers all of it and a floating
  // button over a native window would be a second menu disagreeing with the first.
  // The panels stay because they are not chrome: they are the clipboard editor and
  // the display list themselves, and rebuilding those natively would be two of
  // each. See nativeHost.ts.
  chromeless?: boolean;
  // Hands the panel actions out so the menu bar can drive them, and `null` on
  // unmount. Only the shell passes it.
  onPanelControls?: (controls: PanelControls | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);
  const { panel, setPanel, closePanel, togglePanel } = usePanel();
  // True between pressing Clipboard and the remote's text arriving. The panel
  // stays closed for that moment so it never opens on stale text that visibly
  // rewrites itself a beat later.
  const [clipboardPending, setClipboardPending] = useState(false);
  // null = not yet moved; resolvedPosition falls back to the top-right corner.
  const [position, setPosition] = useState<Position | null>(null);
  const [dragging, setDragging] = useState(false);
  const [viewport, setViewport] = useState<Viewport>(readViewport);

  const dragStateRef = useRef<DragState | null>(null);
  // A drag ends with a synthetic click on some platforms; swallow it so a drag
  // never toggles the toolbar.
  const suppressClickRef = useRef(false);

  useEffect(() => {
    const update = () => {
      const next = readViewport();
      setViewport((prev) =>
        prev.width === next.width &&
        prev.height === next.height &&
        prev.offsetX === next.offsetX &&
        prev.offsetY === next.offsetY
          ? prev
          : next,
      );
    };
    window.addEventListener("resize", update);
    const vp = window.visualViewport;
    vp?.addEventListener("resize", update);
    vp?.addEventListener("scroll", update);
    return () => {
      window.removeEventListener("resize", update);
      vp?.removeEventListener("resize", update);
      vp?.removeEventListener("scroll", update);
    };
  }, []);

  const clamp = useCallback(
    (x: number, y: number): Position => {
      const minX = viewport.offsetX + FAB_MARGIN;
      const minY = viewport.offsetY + FAB_MARGIN;
      const maxX =
        viewport.offsetX +
        Math.max(FAB_MARGIN, viewport.width - FAB_SIZE - FAB_MARGIN);
      const maxY =
        viewport.offsetY +
        Math.max(FAB_MARGIN, viewport.height - FAB_SIZE - FAB_MARGIN);
      return {
        x: Math.min(Math.max(x, minX), maxX),
        y: Math.min(Math.max(y, minY), maxY),
      };
    },
    [viewport],
  );

  const defaultPosition = useCallback(
    (): Position =>
      clamp(
        viewport.offsetX + viewport.width - FAB_SIZE - FAB_MARGIN,
        viewport.offsetY + FAB_MARGIN,
      ),
    [clamp, viewport],
  );

  const resolvedPosition = useMemo(
    () => position ?? defaultPosition(),
    [position, defaultPosition],
  );

  // A shrinking viewport (rotation, keyboard) can strand the FAB off-screen;
  // re-clamp whatever position is held.
  useEffect(() => {
    setPosition((prev) => (prev ? clamp(prev.x, prev.y) : prev));
  }, [clamp]);

  // Escape dismisses the gesture-help overlay, matching the backdrop tap and
  // the Close button. Listener lives only while the overlay is open.
  useEffect(() => {
    if (!helpOpen) {
      return;
    }
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setHelpOpen(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [helpOpen]);

  // Capture the non-persisted chrome shortcut before remote input forwarding.
  const [hidden, setHidden] = useState(false);
  useEffect(() => {
    // Nothing to hide, and the chord would be one this page must not eat: with a
    // menu bar on screen every key belongs to the remote.
    if (chromeless) {
      return;
    }
    const onKeyDown = (e: KeyboardEvent) => {
      // The host's own middle modifier and pointedly not the other one, which is
      // what leaves the four-modifier hyper chord to its owner. See
      // hideChromeShortcut.
      const middle = isMacHost
        ? e.metaKey && !e.altKey
        : e.altKey && !e.metaKey;
      if (e.code !== "Semicolon" || !e.ctrlKey || !e.shiftKey || !middle) {
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      // Command is held and the key it was held with has just been taken by this
      // client, so the input path never saw the chord and would read Command's
      // release as a bare tap — which is how the guest's Start menu opens. Hiding
      // a button must not do that. See macKeys.ts.
      if (isMacHost) {
        onLocalShortcut();
      }
      setHidden((was) => !was);
    };
    window.addEventListener("keydown", onKeyDown, { capture: true });
    return () =>
      window.removeEventListener("keydown", onKeyDown, { capture: true });
  }, [isMacHost, onLocalShortcut, chromeless]);

  const onPointerDown = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => {
      if (
        e.button !== 0 &&
        e.pointerType !== "touch" &&
        e.pointerType !== "pen"
      ) {
        return;
      }
      const current = position ?? defaultPosition();
      dragStateRef.current = {
        pointerId: e.pointerId,
        startX: e.clientX,
        startY: e.clientY,
        originX: current.x,
        originY: current.y,
        dragged: false,
      };
      e.currentTarget.setPointerCapture(e.pointerId);
    },
    [position, defaultPosition],
  );

  const onPointerMove = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => {
      const drag = dragStateRef.current;
      if (!drag || drag.pointerId !== e.pointerId) {
        return;
      }
      const dx = e.clientX - drag.startX;
      const dy = e.clientY - drag.startY;
      if (!drag.dragged && Math.hypot(dx, dy) >= DRAG_THRESHOLD) {
        drag.dragged = true;
        setDragging(true);
      }
      if (!drag.dragged) {
        return;
      }
      setPosition(clamp(drag.originX + dx, drag.originY + dy));
      suppressClickRef.current = true;
      e.preventDefault();
    },
    [clamp],
  );

  const endDrag = useCallback((pointerId: number) => {
    const drag = dragStateRef.current;
    if (!drag || drag.pointerId !== pointerId) {
      return;
    }
    dragStateRef.current = null;
    setDragging(false);
    if (drag.dragged) {
      // Touch may never fire the click that clears the guard; drop it on a
      // timer so the next tap isn't swallowed.
      setTimeout(() => {
        suppressClickRef.current = false;
      }, 400);
    }
  }, []);

  const onPointerUp = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => endDrag(e.pointerId),
    [endDrag],
  );
  const onPointerCancel = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => endDrag(e.pointerId),
    [endDrag],
  );

  const onClick = useCallback(() => {
    if (suppressClickRef.current) {
      suppressClickRef.current = false;
      return;
    }
    setOpen((prev) => !prev);
  }, []);

  // Open the on-screen keyboard and collapse the drawer so the panel has the
  // screen to itself; toggling the button again closes the panel.
  const onSoftKeyboard = useCallback(() => {
    togglePanel("keyboard");
    setOpen(false);
  }, [togglePanel]);

  // Same deal for the clipboard panel, except it cannot open straight away: it
  // fetches first and waits for the answer, so it appears already showing what
  // the remote holds right now. Without that it would open on whatever arrived
  // last — which is nothing at all for a browser that attached mid-session,
  // since it missed every push that came before it.
  const onClipboard = useCallback(() => {
    setOpen(false);
    if (panel === "clipboard") {
      closePanel();
      return;
    }
    if (clipboardPending) {
      return; // a second press while the first is still in flight
    }
    const panelAtFetchStart = panel;
    setClipboardPending(true);
    void onFetchClipboard().finally(() => {
      setClipboardPending(false);
      // Opened even when nothing answered: the panel reports the empty result
      // and its own Fetch is right there to retry.
      setPanel((current) =>
        current === panelAtFetchStart ? "clipboard" : current,
      );
    });
  }, [panel, clipboardPending, onFetchClipboard, closePanel, setPanel]);

  // The same three actions the drawer's buttons take, published for a host whose
  // menus replace the drawer. Republished when they change, and withdrawn on
  // unmount so a menu item cannot open a panel belonging to a session that ended.
  useEffect(() => {
    if (!onPanelControls) {
      return;
    }
    const toggleDisplays = () => {
      setOpen(false);
      togglePanel("display");
    };
    onPanelControls({
      openClipboard: onClipboard,
      toggleDisplays,
      closePanel,
      openHelp: () => {
        setOpen(false);
        setHelpOpen(true);
      },
    });
    return () => onPanelControls(null);
  }, [onPanelControls, onClipboard, togglePanel, closePanel]);

  // The drawer anchors to the FAB: right-aligned to it, placed below unless the
  // FAB sits too low, in which case it flips above.
  const toolbarStyle = useMemo(() => {
    const minLeft = viewport.offsetX + FAB_MARGIN;
    const maxLeft =
      viewport.offsetX +
      Math.max(FAB_MARGIN, viewport.width - TOOLBAR_WIDTH - FAB_MARGIN);
    const desiredLeft = resolvedPosition.x + FAB_SIZE - TOOLBAR_WIDTH;
    const left = Math.min(Math.max(desiredLeft, minLeft), maxLeft);

    const topBelow = resolvedPosition.y + FAB_SIZE + TOOLBAR_GAP;
    const topAbove = resolvedPosition.y - TOOLBAR_GAP;
    const availableBelow =
      viewport.offsetY + viewport.height - topBelow - FAB_MARGIN;
    const availableAbove = topAbove - viewport.offsetY - FAB_MARGIN;
    const placeBelow =
      availableBelow >= TOOLBAR_MIN_HEIGHT || availableBelow >= availableAbove;
    const maxHeight = Math.max(
      TOOLBAR_MIN_HEIGHT,
      Math.floor(placeBelow ? availableBelow : availableAbove),
    );

    return placeBelow
      ? { left: `${left}px`, top: `${topBelow}px`, maxHeight: `${maxHeight}px` }
      : {
          left: `${left}px`,
          top: `${topAbove}px`,
          transform: "translateY(-100%)",
          maxHeight: `${maxHeight}px`,
        };
  }, [resolvedPosition, viewport]);

  return (
    <>
      {/* The button and its drawer go together: a toolbar anchored to a button
          that isn't there reads as a bug. Both keep their state while hidden, so
          the chord brings back exactly what was on screen. Docked panels are left
          alone — they carry their own Close, and in a chromeless host they are the
          only thing this component renders. */}
      {!hidden && !chromeless && (
        <button
          type="button"
          className={`fab${open ? " fab-open" : ""}${dragging ? " fab-dragging" : ""}`}
          style={{
            left: `${resolvedPosition.x}px`,
            top: `${resolvedPosition.y}px`,
          }}
          onClick={onClick}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerCancel}
          aria-label={open ? "Close menu" : "Open menu"}
          aria-expanded={open}
          // The only place the chord is written down in the UI, and it has to be
          // here: once the button is hidden there is nothing left to read it off.
          title={`${hideChromeShortcut(isMacHost)} hides this button`}
        >
          {open ? "✕" : "☰"}
        </button>
      )}

      {open && !hidden && !chromeless && (
        <div className="toolbar" style={toolbarStyle}>
          <DisplaySection
            displays={displays}
            activeDisplayId={activeDisplayId}
            open={panel === "display"}
            onToggle={() => {
              setOpen(false);
              togglePanel("display");
            }}
          />

          <div className="toolbar-section">
            <span className="toolbar-label">Clipboard</span>
            {/* Enabled per target (`clipboard = true`), which every protocol
                supports; a target that didn't opt in leaves this disabled. */}
            <button
              type="button"
              className="toolbar-btn"
              onClick={onClipboard}
              disabled={!canClipboard || clipboardPending}
              aria-pressed={panel === "clipboard"}
              aria-busy={clipboardPending}
              title={
                canClipboard
                  ? "Read and write the remote's clipboard"
                  : "Clipboard sync is not enabled for this target"
              }
            >
              {clipboardPending
                ? "Fetching…"
                : panel === "clipboard"
                  ? "Hide clipboard"
                  : "Clipboard"}
            </button>
          </div>

          <AudioSection
            available={canAudio}
            enabled={audioEnabled}
            error={audioError}
            onChange={onAudioChange}
          />

          <div className="toolbar-section">
            <span className="toolbar-label">Special keys</span>
            <div className="toolbar-keys">
              {SPECIAL_KEYS.map((key) => (
                <button
                  key={key.label}
                  type="button"
                  className="toolbar-btn toolbar-btn-key"
                  onClick={() => sendKeyCombo(key.codes)}
                  title={`Send ${key.label} to the remote`}
                >
                  {key.label}
                </button>
              ))}
            </div>
          </div>

          <div className="toolbar-section">
            <span className="toolbar-label">Modifier tap</span>
            <div className="toolbar-keys">
              {MODIFIER_TAPS.map((mod) => (
                <button
                  key={mod.label}
                  type="button"
                  className="toolbar-btn toolbar-btn-key"
                  onClick={() => sendKeyCombo([mod.code])}
                  title={`Tap ${mod.label}`}
                >
                  {mod.label}
                </button>
              ))}
            </div>
          </div>

          <MacKeyboardSection
            enabled={macKeyOverridesEnabled}
            active={macKeyOverridesActive}
            isMacHost={isMacHost}
            remoteIsMac={remoteIsMac}
            onChange={onMacKeyOverridesChange}
          />

          <div className="toolbar-section toolbar-actions">
            <ImmersiveButton onToggle={() => setOpen(false)} />
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => setHelpOpen(true)}
              title="This session's size and density, and the touch gestures"
            >
              Help
            </button>
            <button
              type="button"
              className="toolbar-btn"
              onClick={onSoftKeyboard}
              aria-pressed={panel === "keyboard"}
            >
              {panel === "keyboard" ? "Hide keyboard" : "Soft keyboard"}
            </button>
            <button
              type="button"
              className="toolbar-btn"
              onClick={onSwitchTarget}
              title="Disconnect and return to the target picker"
            >
              Switch target
            </button>
            <button
              type="button"
              className="toolbar-btn toolbar-btn-danger"
              onClick={onLogout}
            >
              Log out
            </button>
          </div>
        </div>
      )}

      {helpOpen && (
        // biome-ignore lint/a11y/useKeyWithClickEvents: tap-outside dismiss; the Close button covers keyboard users
        // biome-ignore lint/a11y/noStaticElementInteractions: overlay backdrop
        <div className="help-overlay" onClick={() => setHelpOpen(false)}>
          {/* biome-ignore lint/a11y/useKeyWithClickEvents: inner card only stops the backdrop's dismiss */}
          {/* biome-ignore lint/a11y/noStaticElementInteractions: inner card */}
          <div className="help-card" onClick={(e) => e.stopPropagation()}>
            <h2>Help</h2>
            <ScreenHelp
              size={size}
              hostScale={hostScale}
              connection={connection}
              renderPlan={renderPlan}
            />
            {/* Only where there is a menu to hide. A chromeless host has no ☰ and
                does not take the chord (the listener returns early there), so this
                row would be documenting a shortcut that does nothing. */}
            {!chromeless && (
              <>
                <h3>Shortcuts</h3>
                <dl className="help-list">
                  <div className="help-item">
                    <dt>Hide or show this menu</dt>
                    {/* Worth documenting precisely because of what it does: once the
                    ☰ button is hidden there is nothing left on screen to read the
                    way back off, so a shortcut nobody wrote down is a menu that
                    looks gone for good. */}
                    <dd>{hideChromeShortcut(isMacHost)}</dd>
                  </div>
                  <ImmersiveHelpRow />
                  <AppWindowHelpRow />
                </dl>
              </>
            )}
            {isMacHost && (
              <>
                <h3>Mac key override</h3>
                <dl className="help-list">
                  {MAC_KEY_HELP.map((row) => (
                    <div key={row.situation} className="help-item">
                      <dt>{row.situation}</dt>
                      <dd>{row.effect}</dd>
                    </div>
                  ))}
                </dl>
              </>
            )}
            <h3>Touch gestures</h3>
            <dl className="help-list">
              {GESTURE_HELP.map((row) => (
                <div key={row.gesture} className="help-item">
                  <dt>{row.gesture}</dt>
                  <dd>{row.action}</dd>
                </div>
              ))}
            </dl>
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => setHelpOpen(false)}
            >
              Close
            </button>
          </div>
        </div>
      )}

      <DockedPanel
        panel={panel}
        onClose={closePanel}
        onDockedHeightChange={onKeyboardInset}
        sendKeyCombo={sendKeyCombo}
        remoteClipboard={remoteClipboard}
        onSendClipboard={onSendClipboard}
        displays={displays}
        activeDisplayId={activeDisplayId}
        onSelectDisplay={onSelectDisplay}
      />
    </>
  );
}
