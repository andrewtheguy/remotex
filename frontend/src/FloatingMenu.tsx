import {
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { SoftKeyboardPanel } from "./SoftKeyboardPanel.tsx";

// Phase 9: the floating chrome — a draggable ☰ button that toggles a toolbar
// drawer. The drawer carries this project's controls (browser-swallowed keys,
// modifier taps, the gesture cheat-sheet), a Switch target button that returns
// to the post-login picker, and the logout affordance that used to live in the
// Ctrl+Alt+Shift+L chord and the below-canvas bar. Phase 10 wired the drawer's
// Soft keyboard button to the on-screen keyboard panel; Clipboard is still a
// placeholder until its phase lands.
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

export default function FloatingMenu({
  onLogout,
  onSwitchTarget,
  sendKeyCombo,
  onKeyboardInset,
}: {
  onLogout: () => void;
  // Return to the post-login target picker ("switch target"): disconnects the
  // current session without ending the login. See useRemoteDesktop.
  onSwitchTarget: () => void;
  sendKeyCombo: (codes: string[]) => void;
  // Reports the docked soft keyboard's height so the touch canvas can inset
  // above it (0 when the panel closes or floats). See useRemoteDesktop.
  onKeyboardInset: (px: number) => void;
}) {
  const [open, setOpen] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);
  const [keyboardOpen, setKeyboardOpen] = useState(false);
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
    setKeyboardOpen((prev) => !prev);
    setOpen(false);
  }, []);

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
            {/* Stub: clipboard relay isn't in the wire protocol yet — the VNC
                engine drains and drops ServerCutText. Disabled so it reads as
                not-yet-live until a later phase wires it up. */}
            <button
              type="button"
              className="toolbar-btn"
              disabled
              title="Clipboard sync — not implemented yet"
            >
              Clipboard (soon)
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

          <div className="toolbar-section toolbar-actions">
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
              aria-pressed={keyboardOpen}
            >
              {keyboardOpen ? "Hide keyboard" : "Soft keyboard"}
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
              Disconnect
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

      {keyboardOpen && (
        <SoftKeyboardPanel
          sendKeyCombo={sendKeyCombo}
          onClose={() => setKeyboardOpen(false)}
          onDockedHeightChange={onKeyboardInset}
        />
      )}
    </>
  );
}
