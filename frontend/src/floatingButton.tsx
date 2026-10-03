import {
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

// The floating chrome's button and where its drawer goes — a draggable ☰ button
// that toggles a toolbar drawer anchored to it. The session's page and a second
// display's tab each have one, with a drawer of their own: see FloatingMenu.tsx
// and DisplayMenu.tsx.
const FAB_SIZE = 40;
const FAB_MARGIN = 12;
// Pointer travel (px) before a press becomes a drag rather than a click.
const DRAG_THRESHOLD = 6;
const TOOLBAR_WIDTH = 240;
const TOOLBAR_GAP = 10;
const TOOLBAR_MIN_HEIGHT = 120;

// The chrome shortcut, spelled the way the Help card and the button's tooltip show
// it. One source for all three so the card, the tooltip and the handler cannot
// drift — the handler matches exactly what this returns.
//
// The middle modifier is the host's: Command on a Mac, where Option is a
// character-composing key and Ctrl+Alt chords are Windows vocabulary, and Alt
// everywhere else. Whichever it is, the *other* one has to be absent, so all four
// held — a Mac user's own Cmd+Ctrl+Alt+Shift+; hyper chord — stays theirs.
export function hideChromeShortcut(isMacHost: boolean): string {
  return isMacHost ? "Ctrl + Cmd + Shift + ;" : "Ctrl + Alt + Shift + ;";
}

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

// What a page's button hands back: whether its drawer is open, whether the chord
// has hidden both, where the drawer goes, and the button itself.
export interface FloatingButtonState {
  open: boolean;
  setOpen: (open: boolean) => void;
  hidden: boolean;
  toolbarStyle: CSSProperties;
  button: FloatingButtonProps;
}

interface FloatingButtonProps {
  glyph: string;
  open: boolean;
  dragging: boolean;
  position: Position;
  title: string;
  onClick: () => void;
  onPointerDown: (e: ReactPointerEvent<HTMLButtonElement>) => void;
  onPointerMove: (e: ReactPointerEvent<HTMLButtonElement>) => void;
  onPointerUp: (e: ReactPointerEvent<HTMLButtonElement>) => void;
  onPointerCancel: (e: ReactPointerEvent<HTMLButtonElement>) => void;
}

export function useFloatingButton({
  glyph,
  dockedHeight,
  isMacHost,
  onLocalShortcut,
}: {
  // What the closed button shows, which is how the two pages' buttons are told
  // apart: ☰ on the session's page, the display's number on a display's tab.
  glyph: string;
  // How much of the bottom edge a docked panel is covering, which the button and
  // its drawer stay above. Zero on a page with no panels.
  dockedHeight: number;
  isMacHost: boolean;
  // A chord taken here, announced to the input path. See useRemoteDesktop.
  onLocalShortcut: () => void;
}): FloatingButtonState {
  const [open, setOpen] = useState(false);
  // null = not yet moved; resolvedPosition falls back to the lower-right corner.
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

  // The lowest edge the floating chrome may reach. `visualViewport` already
  // takes the browser's own on-screen keyboard off the bottom; a docked panel
  // is drawn by this page, so nothing but this subtracts it.
  const floor = viewport.offsetY + viewport.height - dockedHeight;

  const clamp = useCallback(
    (x: number, y: number): Position => {
      const minX = viewport.offsetX + FAB_MARGIN;
      const minY = viewport.offsetY + FAB_MARGIN;
      const maxX =
        viewport.offsetX +
        Math.max(FAB_MARGIN, viewport.width - FAB_SIZE - FAB_MARGIN);
      const maxY = Math.max(minY, floor - FAB_SIZE - FAB_MARGIN);
      return {
        x: Math.min(Math.max(x, minX), maxX),
        y: Math.min(Math.max(y, minY), maxY),
      };
    },
    [viewport, floor],
  );

  const defaultPosition = useCallback(
    (): Position =>
      clamp(
        viewport.offsetX + viewport.width - FAB_SIZE - FAB_MARGIN,
        floor - FAB_SIZE - FAB_MARGIN,
      ),
    [clamp, viewport, floor],
  );

  // Clamped on the way out rather than in place, so that what a drag stored
  // survives a floor that only moved for a moment: a rotation, or a panel
  // opening, bumps the button up while it lasts, and giving the room back puts
  // it where its owner left it. Every reader — the drawer's anchor, a drag's
  // origin — takes this and not the raw state, so nothing jumps.
  const resolvedPosition = useMemo(
    () => (position ? clamp(position.x, position.y) : defaultPosition()),
    [position, clamp, defaultPosition],
  );

  // Capture the non-persisted chrome shortcut before remote input forwarding.
  const [hidden, setHidden] = useState(false);
  useEffect(() => {
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
  }, [isMacHost, onLocalShortcut]);

  const onPointerDown = useCallback(
    (e: ReactPointerEvent<HTMLButtonElement>) => {
      if (
        e.button !== 0 &&
        e.pointerType !== "touch" &&
        e.pointerType !== "pen"
      ) {
        return;
      }
      dragStateRef.current = {
        pointerId: e.pointerId,
        startX: e.clientX,
        startY: e.clientY,
        originX: resolvedPosition.x,
        originY: resolvedPosition.y,
        dragged: false,
      };
      e.currentTarget.setPointerCapture(e.pointerId);
    },
    [resolvedPosition],
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
    const availableBelow = floor - topBelow - FAB_MARGIN;
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
  }, [resolvedPosition, viewport, floor]);

  return {
    open,
    setOpen,
    hidden,
    toolbarStyle,
    button: {
      glyph,
      open,
      dragging,
      position: resolvedPosition,
      // The only place the chord is written down in the UI, and it has to be
      // here: once the button is hidden there is nothing left to read it off.
      title: `${hideChromeShortcut(isMacHost)} hides this button`,
      onClick,
      onPointerDown,
      onPointerMove,
      onPointerUp,
      onPointerCancel,
    },
  };
}

export function FloatingButton({
  glyph,
  open,
  dragging,
  position,
  title,
  onClick,
  onPointerDown,
  onPointerMove,
  onPointerUp,
  onPointerCancel,
}: FloatingButtonProps) {
  return (
    <button
      type="button"
      className={`fab${open ? " fab-open" : ""}${dragging ? " fab-dragging" : ""}`}
      style={{ left: `${position.x}px`, top: `${position.y}px` }}
      onClick={onClick}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      aria-label={open ? "Close menu" : "Open menu"}
      aria-expanded={open}
      title={title}
    >
      {open ? "✕" : glyph}
    </button>
  );
}
