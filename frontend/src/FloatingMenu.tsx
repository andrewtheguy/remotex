import {
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { ClipboardPanel } from "./ClipboardPanel.tsx";
import { ResolutionPanel } from "./ResolutionPanel.tsx";
import { SoftKeyboardPanel } from "./SoftKeyboardPanel.tsx";

// Phase 9: the floating chrome — a draggable ☰ button that toggles a toolbar
// drawer. The drawer carries this project's controls (browser-swallowed keys,
// modifier taps, the gesture cheat-sheet), a Switch target button that returns
// to the post-login picker, and the Log out affordance (ending the web login)
// that used to live in the Ctrl+Alt+Shift+L chord and the below-canvas bar.
// Phase 10 wired the drawer's Soft keyboard button to the on-screen keyboard
// panel; the Clipboard button opens the clipboard bridge's panel, and is
// enabled only for targets that opted into it (see ClipboardPanel). The
// Resolution button opens the third such panel, for targets that offer a list
// of display modes (see ResolutionPanel).
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
// One state rather than a boolean each: all three dock to the bottom edge and
// report the same canvas inset, so a second one open would sit on the first.
// Three booleans made that a rule every call site had to remember — open this
// one, clear the other two — and this makes it impossible to express.
type Panel = "clipboard" | "keyboard" | "resolution";

function usePanel() {
  const [panel, setPanel] = useState<Panel | null>(null);
  const closePanel = useCallback(() => setPanel(null), []);
  const togglePanel = useCallback(
    (next: Panel) => setPanel((prev) => (prev === next ? null : next)),
    [],
  );
  return { panel, setPanel, closePanel, togglePanel };
}

// Whichever docked panel is open, or nothing. Rendering all three from one
// place is what makes the shared inset channel safe: exactly one of them is
// mounted, so exactly one is reporting a height.
function DockedPanel({
  panel,
  onClose,
  onDockedHeightChange,
  sendKeyCombo,
  modes,
  remoteSize,
  onPickResolution,
  remoteClipboard,
  onFetchClipboard,
  onSendClipboard,
}: {
  panel: Panel | null;
  onClose: () => void;
  onDockedHeightChange: (px: number) => void;
  sendKeyCombo: (codes: string[]) => void;
  modes: { w: number; h: number }[];
  remoteSize: { w: number; h: number } | null;
  onPickResolution: (w: number, h: number) => void;
  remoteClipboard: { text: string; seq: number } | null;
  onFetchClipboard: () => Promise<string | null>;
  onSendClipboard: (text: string) => void;
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
    case "resolution":
      return (
        <ResolutionPanel
          modes={modes}
          current={remoteSize}
          onPick={onPickResolution}
          onClose={onClose}
          onDockedHeightChange={onDockedHeightChange}
        />
      );
    case "clipboard":
      return (
        <ClipboardPanel
          onFetch={onFetchClipboard}
          onSend={onSendClipboard}
          remoteClipboard={remoteClipboard}
          onClose={onClose}
          onDockedHeightChange={onDockedHeightChange}
        />
      );
    default:
      return null;
  }
}

// The drawer's way in to the resolution panel: one button, labelled with the
// size the remote is at now.
//
// Only a Mac (rxa) target sharing a virtual display offers a list, so this
// renders nothing for every other target — including a Mac on a physical
// display, which is the point: a real monitor is never rearranged from here.
function ResolutionButton({
  modes,
  current,
  panelOpen,
  onToggle,
}: {
  modes: { w: number; h: number }[];
  current: { w: number; h: number } | null;
  panelOpen: boolean;
  onToggle: () => void;
}) {
  if (modes.length === 0) {
    return null;
  }
  return (
    <div className="toolbar-section">
      <span className="toolbar-label">Resolution</span>
      <button
        type="button"
        className="toolbar-btn"
        onClick={onToggle}
        aria-pressed={panelOpen}
        title="Set the remote display to one of its supported sizes"
      >
        {panelOpen
          ? "Hide resolutions"
          : current
            ? `${current.w} × ${current.h}`
            : "Resolution"}
      </button>
    </div>
  );
}

export default function FloatingMenu({
  onLogout,
  onSwitchTarget,
  onResizeToWindow,
  displayModes,
  remoteSize,
  onSetResolution,
  sendKeyCombo,
  onKeyboardInset,
  canClipboard,
  remoteClipboard,
  onFetchClipboard,
  onSendClipboard,
}: {
  onLogout: () => void;
  // Return to the post-login target picker ("switch target"): disconnects the
  // current session without ending the login. See useRemoteDesktop.
  onSwitchTarget: () => void;
  // Resize the remote desktop to the browser window. Present only when the
  // target supports on-request resize (RDP with resize enabled); VNC follows
  // the viewport automatically and needs no button. See useRemoteDesktop.
  onResizeToWindow?: () => void;
  // The resolutions the remote will accept, largest first. Non-empty only for
  // a Mac (rxa) target sharing a virtual display with `resize = true`: such a
  // display takes sizes off a fixed list rather than following the window, so
  // it gets a menu instead of onResizeToWindow's button. Empty hides the
  // section entirely.
  displayModes: { w: number; h: number }[];
  // The remote's current size, so the menu can mark which entry is in effect.
  // Null before the first frame.
  remoteSize: { w: number; h: number } | null;
  onSetResolution: (w: number, h: number) => void;
  sendKeyCombo: (codes: string[]) => void;
  // Reports the open docked panel's height so the touch canvas can inset above
  // it (0 when the panel closes or floats). See useRemoteDesktop. All three
  // panels share this channel, which is safe because only one is ever open.
  onKeyboardInset: (px: number) => void;
  // Whether the connected target opted into the clipboard bridge
  // (`clipboard = true`). False leaves the Clipboard button disabled.
  canClipboard: boolean;
  // The last clipboard reply from the server, and the fetch actions. See
  // ClipboardPanel — the browser holds no clipboard state of its own.
  // `onFetchClipboard` resolves with the remote's text, or null if nothing
  // answered.
  remoteClipboard: { text: string; seq: number } | null;
  onFetchClipboard: () => Promise<string | null>;
  onSendClipboard: (text: string) => void;
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
    setClipboardPending(true);
    void onFetchClipboard().finally(() => {
      setClipboardPending(false);
      // Opened even when nothing answered: the panel reports the empty result
      // and its own Fetch is right there to retry.
      setPanel("clipboard");
    });
  }, [panel, clipboardPending, onFetchClipboard, closePanel, setPanel]);

  // The resolution list opens as a panel rather than sitting in the drawer:
  // it is per-display and can run to a dozen entries, which pushed everything
  // below it out of reach. Same deal as the other two panels — get the drawer
  // out of the way, and take the bottom edge for itself.
  const onResolution = useCallback(() => {
    setOpen(false);
    togglePanel("resolution");
  }, [togglePanel]);

  // Resize the remote desktop to the window, then collapse the drawer so the
  // resized desktop is visible.
  const onResize = useCallback(() => {
    onResizeToWindow?.();
    setOpen(false);
  }, [onResizeToWindow]);

  // Same for picking a resolution: the point of the click is to look at the
  // resized desktop, which the panel would be sitting on top of.
  const onPickResolution = useCallback(
    (w: number, h: number) => {
      onSetResolution(w, h);
      closePanel();
    },
    [onSetResolution, closePanel],
  );

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
      >
        {open ? "✕" : "☰"}
      </button>

      {open && (
        <div className="toolbar" style={toolbarStyle}>
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

          <ResolutionButton
            modes={displayModes}
            current={remoteSize}
            panelOpen={panel === "resolution"}
            onToggle={onResolution}
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
        modes={displayModes}
        remoteSize={remoteSize}
        onPickResolution={onPickResolution}
        remoteClipboard={remoteClipboard}
        onFetchClipboard={onFetchClipboard}
        onSendClipboard={onSendClipboard}
      />
    </>
  );
}
