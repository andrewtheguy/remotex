import {
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { ClipboardPanel } from "./ClipboardPanel.tsx";
import DisplayPanel from "./DisplayPanel.tsx";
import type {
  ClipboardSnapshot,
  DisplayInfo,
  RemoteClipboard,
} from "./protocol.ts";
import { SoftKeyboardPanel } from "./SoftKeyboardPanel.tsx";

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

// The Command-to-Control switch, on Mac hosts only: nothing else has a Command
// key to reinterpret, and a section explaining that to a Windows user is worse
// than no section. Disabled rather than hidden for a Mac guest, matching the
// viewer's menu item — the preference still exists, it just has nothing to do
// while Command already means Command at the other end.
//
// The label names the state rather than the action, and says "on"/"off" out loud
// to do it. `aria-pressed` is the honest place for that, but nothing in the
// stylesheet draws a pressed toolbar button, so the text is the only channel a
// sighted user has — which is why the other toggles here spell their state as
// "Hide displays" and "Hide keyboard".
//
// This one cannot borrow that trick. "Hide" is unmistakably an action, whereas
// "⌘ → Ctrl" describes a key mapping, so as a bare label it reads as what pressing
// the button would *do* rather than what is already happening — and it meant the
// opposite of that. The viewer has no such problem: AppKit puts a checkmark beside
// a fixed "Enable macOS Keyboard Overrides", so its name never has to move.
//
// Three states are still worth distinguishing, and a Mac guest is the one that is
// genuinely not a state of this preference but a reason it has nothing to do. See
// macKeys.ts.
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
  const label = remoteIsMac
    ? "⌘ stays ⌘"
    : active
      ? "⌘ → Ctrl: on"
      : "⌘ → Ctrl: off";
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
  onResizeToWindow,
  sendKeyCombo,
  onKeyboardInset,
  canClipboard,
  remoteClipboard,
  onFetchClipboard,
  onSendClipboard,
  displays,
  activeDisplayId,
  onSelectDisplay,
  macKeyOverridesEnabled,
  macKeyOverridesActive,
  isMacHost,
  remoteIsMac,
  onMacKeyOverridesChange,
}: {
  onLogout: () => void;
  // Return to the post-login target picker ("switch target"): disconnects the
  // current session without ending the login. See useRemoteDesktop.
  onSwitchTarget: () => void;
  // Resize the remote desktop to the browser window. Present only for an RDP
  // target with `resize = true`, whose protocol puts the desktop size in the
  // client's hands but makes changing it expensive enough to be worth asking
  // for. VNC follows the viewport automatically and needs no button; every
  // other remote's resolution is set on the remote. See useRemoteDesktop.
  onResizeToWindow?: () => void;
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
  // The Command-to-Control preference and whether it is doing anything. The two
  // differ when the guest is itself a Mac, which is why the section reports the
  // reason rather than just showing the switch off. The whole section is absent
  // on a non-Mac host, where there is no Command key to translate. See macKeys.ts.
  macKeyOverridesEnabled: boolean;
  macKeyOverridesActive: boolean;
  isMacHost: boolean;
  remoteIsMac: boolean;
  onMacKeyOverridesChange: (enabled: boolean) => void;
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

  // Ctrl+Alt+Shift+Semicolon hides the floating chrome and shows it again — for a
  // screen recording, or to uncover whatever the button is sitting on top of.
  //
  // Two things make the chord workable. It is caught on `window` in the **capture**
  // phase and stopped there, because the remote surface forwards every key it sees
  // to the guest (see useRemoteDesktop): a bubble-phase listener would toggle the
  // button *and* type the chord at the remote. And capturing on the window rather
  // than the surface means it still works when focus has wandered off the desktop,
  // which matters when the way back is the only way back.
  //
  // Three modifiers because the chord this steals from is the guest's, not the
  // browser's — whatever it binds never reaches the remote again. Ctrl+Shift+; was
  // tried and is Excel's "insert the current time", and Ctrl+Alt+anything is AltGr
  // on Windows and X11, so on a non-US layout it can be a character somebody means
  // to type. Adding Shift lands on a fourth modifier level that few layouts
  // populate, and no browser or desktop binds.
  //
  // Deliberately not persisted: a chrome-less desktop with no visible way to
  // recover it should not survive a reload.
  const [hidden, setHidden] = useState(false);
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      // All three and no Command: a Mac user's Cmd+Ctrl+Alt+Shift+; stays theirs.
      if (
        e.code !== "Semicolon" ||
        !e.ctrlKey ||
        !e.altKey ||
        !e.shiftKey ||
        e.metaKey
      ) {
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      setHidden((was) => !was);
    };
    window.addEventListener("keydown", onKeyDown, { capture: true });
    return () =>
      window.removeEventListener("keydown", onKeyDown, { capture: true });
  }, []);

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

  // Resize the remote desktop to the window, then collapse the drawer so the
  // resized desktop is visible.
  const onResize = useCallback(() => {
    onResizeToWindow?.();
    setOpen(false);
  }, [onResizeToWindow]);

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
          alone — they carry their own Close. */}
      {!hidden && (
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
          title="Ctrl+Alt+Shift+; hides this button"
        >
          {open ? "✕" : "☰"}
        </button>
      )}

      {open && !hidden && (
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
            {onResizeToWindow && (
              <button
                type="button"
                className="toolbar-btn"
                onClick={onResize}
                title="Resize the remote desktop to the browser window"
              >
                Resize to window
              </button>
            )}
            <button
              type="button"
              className="toolbar-btn"
              onClick={() => setHelpOpen(true)}
            >
              Gestures
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
            <h2>Touch gestures</h2>
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
