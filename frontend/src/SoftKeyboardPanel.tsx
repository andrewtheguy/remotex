import {
  type RefObject,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import {
  DESKTOP_ARROW_ROW_1,
  DESKTOP_ARROW_ROW_2,
  DESKTOP_BOTTOM_LEFT,
  DESKTOP_BOTTOM_RIGHT,
  DESKTOP_FUNCTION_ROW,
  DESKTOP_HOME_ROW,
  DESKTOP_NAV_ROW_1,
  DESKTOP_NAV_ROW_2,
  DESKTOP_NUMBER_ROW,
  DESKTOP_QWERTY_ROW,
  DESKTOP_SHIFT_LEFT,
  DESKTOP_SHIFT_RIGHT,
  DESKTOP_SPACE_KEY,
  DESKTOP_ZXCV_ROW,
  FUNCTION_KEY_ROW,
  GUI_COMBO_ROW,
  MODIFIER_KEYS,
  modifierOf,
  PRIMARY_SCREEN_ROWS,
  type PrintableSoftKey,
  ROW_HOME,
  SECONDARY_SCREEN_ROWS,
  type SoftKeyboardScreen,
  type SoftKeyDefinition,
  type SoftKeyModifiers,
  shiftHeld,
} from "./softKeyboard.ts";

// A held non-modifier key repeats after this initial delay, then at this
// interval — matching a physical keyboard's typematic feel.
const REPEAT_DELAY_MS = 400;
const REPEAT_INTERVAL_MS = 80;

// How far a finger may travel *along the scroll axis* before a tap on a
// scrollable row counts as a scroll instead.
//
// The scrollable rows cannot fire on pointer-down the way the fixed rows do —
// that would send a key every time the row is flicked sideways. They used to use
// `onClick`, which has the opposite failure: a click needs the press and release
// on the same element with no scroll intervening, so a tap with a few pixels of
// drift is swallowed and the key never sends at all. Tracking the pointer gives
// both — the row still scrolls, and a tap that stayed put still counts.
//
// Only horizontal travel disqualifies a tap: these rows scroll on one axis, and
// vertical drift is just what a sloppy tap looks like. Keep this at or below
// the browsers' own scroll slop (~8–10px) — the tap also fires on
// `pointercancel` when it stayed inside this budget, so a threshold above the
// browser's would let a slow deliberate scroll send a key.
const SCROLL_DRAG_THRESHOLD_PX = 8;

// A finger that has slid several key-heights *off* the row before lifting is a
// change of mind, not tap jitter — abandon the key instead of firing it.
const VERTICAL_ABANDON_THRESHOLD_PX = 32;

interface SoftKeyboardPanelProps {
  // Presses each DOM code in order then releases in reverse (transient — see
  // useRemoteDesktop.sendKeyCombo). The panel's only channel to the remote.
  sendKeyCombo: (codes: string[]) => void;
  onClose: () => void;
  // Reports the panel's height (CSS px) while it's docked to the bottom edge
  // (mobile), 0 while it floats (desktop) or when it unmounts. Lets the touch
  // canvas pan up above the keyboard instead of hiding under it.
  onDockedHeightChange?: (px: number) => void;
}

// ── Helpers ──

// React key for a definition: a code is unique within a row and, unlike a
// label, tells the desktop grid's two Shifts (or Alts, or Ctrls) apart. Combos
// have no single code, so their label stands in.
function keyOf(def: SoftKeyDefinition): string {
  return def.type === "combo" ? def.label : def.code;
}

// Whether this key is a sticky modifier the panel is currently holding.
function isHeld(def: SoftKeyDefinition, modifiers: SoftKeyModifiers): boolean {
  return (
    modifierOf(def) !== null && def.type !== "combo" && modifiers.has(def.code)
  );
}

function getDisplayLabel(def: SoftKeyDefinition, shift: boolean): string {
  if (def.type === "printable" && shift) {
    return def.shiftLabel ?? def.label.toUpperCase();
  }
  return def.label;
}

// ── SoftKeyButton ──

interface SoftKeyButtonProps {
  def: SoftKeyDefinition;
  // Whether a Shift is held, which decides the glyph shown.
  shift: boolean;
  onPress: (def: SoftKeyDefinition) => void;
  onRelease: (def: SoftKeyDefinition) => void;
  isActive?: boolean;
  scrollable?: boolean;
  extraClass?: string;
}

function SoftKeyButton({
  def,
  shift,
  onPress,
  onRelease,
  isActive,
  scrollable,
  extraClass,
}: SoftKeyButtonProps) {
  const repeatTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const repeatIntervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const pressedRef = useRef(false);
  // Scrollable rows only: where the finger went down, and whether it has since
  // travelled far enough to be a scroll rather than a tap.
  const pointerStartRef = useRef<{ x: number; y: number } | null>(null);
  const draggedRef = useRef(false);

  const clearRepeat = useCallback(() => {
    if (repeatTimerRef.current) {
      clearTimeout(repeatTimerRef.current);
      repeatTimerRef.current = null;
    }
    if (repeatIntervalRef.current) {
      clearInterval(repeatIntervalRef.current);
      repeatIntervalRef.current = null;
    }
  }, []);

  // Cleanup on unmount.
  useEffect(() => clearRepeat, [clearRepeat]);

  const handlePointerDown = useCallback(
    (e: React.PointerEvent) => {
      e.preventDefault();
      pressedRef.current = true;
      onPress(def);

      // Modifiers toggle (no repeat) and combos fire once.
      if (modifierOf(def) || def.type === "combo") {
        return;
      }

      clearRepeat();
      repeatTimerRef.current = setTimeout(() => {
        repeatIntervalRef.current = setInterval(() => {
          onPress(def);
        }, REPEAT_INTERVAL_MS);
      }, REPEAT_DELAY_MS);
    },
    [def, onPress, clearRepeat],
  );

  const stopPress = useCallback(
    (e: React.PointerEvent) => {
      e.preventDefault();
      if (!pressedRef.current) {
        return;
      }
      pressedRef.current = false;
      clearRepeat();
      onRelease(def);
    },
    [def, onRelease, clearRepeat],
  );

  // No `preventDefault` on this path: the row under it has to keep panning, and
  // the key is decided on pointer-up.
  const startScrollableTap = useCallback((e: React.PointerEvent) => {
    pointerStartRef.current = { x: e.clientX, y: e.clientY };
    draggedRef.current = false;
  }, []);

  const trackScrollableTap = useCallback((e: React.PointerEvent) => {
    const start = pointerStartRef.current;
    if (!start || draggedRef.current) {
      return;
    }
    const dx = e.clientX - start.x;
    const dy = e.clientY - start.y;
    if (
      Math.abs(dx) > SCROLL_DRAG_THRESHOLD_PX ||
      Math.abs(dy) > VERTICAL_ABANDON_THRESHOLD_PX
    ) {
      draggedRef.current = true;
    }
  }, []);

  // Decides the tap on pointer-up *and* pointer-cancel. The cancel matters:
  // the browser fires it the moment it claims the gesture for scrolling, and
  // its slop can trip before pointer-up ever arrives — under the old
  // cancel-means-drop rule that tap was silently swallowed. If the finger
  // never crossed the drag threshold, it was a tap, whoever ended it.
  const finishScrollableTap = useCallback(() => {
    if (pointerStartRef.current && !draggedRef.current) {
      onPress(def);
    }
    pointerStartRef.current = null;
  }, [def, onPress]);

  // A mouse pointer that leaves the key mid-press is not a tap on this key.
  // (Touch never gets here mid-gesture: touch pointers are implicitly captured
  // by the element that received pointer-down.)
  const cancelScrollableTap = useCallback(() => {
    pointerStartRef.current = null;
  }, []);

  const label = getDisplayLabel(def, shift);
  const isSingleChar = label.length === 1;
  const widthClass = def.width
    ? `sk-wide-${String(def.width).replace(".", "_")}`
    : "";
  const showShiftHint =
    def.type === "printable" &&
    !shift &&
    def.shiftLabel &&
    def.shiftLabel !== def.label.toUpperCase();

  return (
    <div
      className={`sk-button ${widthClass} ${extraClass ?? ""} ${isActive ? "sk-active" : ""} ${isSingleChar ? "sk-single-char" : ""}`}
      {...(scrollable
        ? {
            onPointerDown: startScrollableTap,
            onPointerMove: trackScrollableTap,
            onPointerUp: finishScrollableTap,
            onPointerLeave: cancelScrollableTap,
            onPointerCancel: finishScrollableTap,
          }
        : {
            onPointerDown: handlePointerDown,
            onPointerUp: stopPress,
            onPointerLeave: stopPress,
            onPointerCancel: stopPress,
          })}
    >
      {label}
      {showShiftHint && (
        <span className="sk-shift-hint">
          {(def as PrintableSoftKey).shiftLabel}
        </span>
      )}
    </div>
  );
}

// ── Viewport detection ──

// Wide viewports render a full PC keyboard grid; narrow ones the compact
// mobile layout with a screen toggle. Exported because every docking panel
// makes the same docked-vs-floating call off the same breakpoint.
export function useIsDesktop(breakpoint = 800): boolean {
  const [desktop, setDesktop] = useState(() => window.innerWidth >= breakpoint);
  useEffect(() => {
    const mql = window.matchMedia(`(min-width: ${breakpoint}px)`);
    const handler = (e: MediaQueryListEvent) => setDesktop(e.matches);
    mql.addEventListener("change", handler);
    return () => mql.removeEventListener("change", handler);
  }, [breakpoint]);
  return desktop;
}

// Report a bottom-docked panel's height so the touch canvas can inset above
// it, and 0 whenever it isn't covering anything — floating on desktop, or
// unmounted. Every panel that docks to the bottom edge shares one inset
// channel, so they have to agree on this exactly; keeping it in one place is
// what makes "only one panel is ever open" safe to rely on.
export function useDockedHeight(
  panelRef: RefObject<HTMLDivElement | null>,
  onDockedHeightChange: ((px: number) => void) | undefined,
) {
  const isDesktop = useIsDesktop();
  useEffect(() => {
    const report = onDockedHeightChange;
    if (!report) {
      return;
    }
    if (isDesktop) {
      report(0);
      return;
    }
    const panel = panelRef.current;
    if (!panel) {
      return;
    }
    const measure = () => report(panel.getBoundingClientRect().height);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(panel);
    return () => {
      observer.disconnect();
      report(0);
    };
  }, [isDesktop, onDockedHeightChange, panelRef]);
}

// ── DesktopKeyboardGrid ──

interface DesktopKeyboardGridProps {
  modifiers: SoftKeyModifiers;
  onPress: (def: SoftKeyDefinition) => void;
  onRelease: (def: SoftKeyDefinition) => void;
}

function DesktopKeyboardGrid({
  modifiers,
  onPress,
  onRelease,
}: DesktopKeyboardGridProps) {
  const shift = shiftHeld(modifiers);
  const renderKey = (def: SoftKeyDefinition, extraClass?: string) => (
    <SoftKeyButton
      key={keyOf(def)}
      def={def}
      shift={shift}
      onPress={onPress}
      onRelease={onRelease}
      isActive={isHeld(def, modifiers)}
      extraClass={extraClass}
    />
  );

  return (
    <div className="sk-desktop-layout">
      <div className="sk-desktop-main">
        <div className="sk-desktop-row sk-desktop-row-fn">
          {DESKTOP_FUNCTION_ROW.map((def) =>
            renderKey(def, def.label === "Esc" ? "sk-dk-esc" : undefined),
          )}
        </div>
        <div className="sk-desktop-row">
          {DESKTOP_NUMBER_ROW.map((def) =>
            renderKey(def, def.label === "Bksp" ? "sk-dk-bksp" : undefined),
          )}
        </div>
        <div className="sk-desktop-row">
          {DESKTOP_QWERTY_ROW.map((def) =>
            renderKey(
              def,
              def.label === "Tab"
                ? "sk-dk-tab"
                : def.label === "\\"
                  ? "sk-dk-slash"
                  : undefined,
            ),
          )}
        </div>
        <div className="sk-desktop-row">
          <div className="sk-dk-home-spacer" />
          {DESKTOP_HOME_ROW.map((def) =>
            renderKey(def, def.label === "Enter" ? "sk-dk-enter" : undefined),
          )}
        </div>
        <div className="sk-desktop-row">
          {renderKey(DESKTOP_SHIFT_LEFT, "sk-dk-shift")}
          {DESKTOP_ZXCV_ROW.map((def) => renderKey(def))}
          {renderKey(DESKTOP_SHIFT_RIGHT, "sk-dk-shift")}
        </div>
        <div className="sk-desktop-row">
          {DESKTOP_BOTTOM_LEFT.map((def) => renderKey(def, "sk-dk-modifier"))}
          {renderKey(DESKTOP_SPACE_KEY, "sk-dk-space")}
          {DESKTOP_BOTTOM_RIGHT.map((def) => renderKey(def, "sk-dk-modifier"))}
        </div>
      </div>
      <div className="sk-desktop-side">
        <div className="sk-desktop-side-panel sk-desktop-nav">
          <div className="sk-desktop-side-row">
            {DESKTOP_NAV_ROW_1.map((def) => renderKey(def, "sk-dk-side"))}
          </div>
          <div className="sk-desktop-side-row">
            {DESKTOP_NAV_ROW_2.map((def) => renderKey(def, "sk-dk-side"))}
          </div>
        </div>
        <div className="sk-desktop-side-panel sk-desktop-arrows">
          <div className="sk-desktop-side-row">
            <div />
            {renderKey(DESKTOP_ARROW_ROW_1[0], "sk-dk-side sk-dk-arrow")}
            <div />
          </div>
          <div className="sk-desktop-side-row">
            {DESKTOP_ARROW_ROW_2.map((def) =>
              renderKey(def, "sk-dk-side sk-dk-arrow"),
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

// ── SoftKeyboardPanel ──

export function SoftKeyboardPanel({
  sendKeyCombo,
  onClose,
  onDockedHeightChange,
}: SoftKeyboardPanelProps) {
  const [modifiers, setModifiers] = useState<SoftKeyModifiers>(() => new Set());
  const [screen, setScreen] = useState<SoftKeyboardScreen>("primary");
  const isDesktop = useIsDesktop();

  // ── Drag state (desktop floating mode) ──
  const panelRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef<{
    pointerId: number;
    offsetX: number;
    offsetY: number;
  } | null>(null);
  const [dragPosition, setDragPosition] = useState<{
    left: number;
    top: number;
  } | null>(null);

  const handleDragStart = useCallback((e: React.PointerEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const panel = panelRef.current;
    if (!panel) {
      return;
    }
    const rect = panel.getBoundingClientRect();
    setDragPosition({ left: rect.left, top: rect.top });
    dragRef.current = {
      pointerId: e.pointerId,
      offsetX: e.clientX - rect.left,
      offsetY: e.clientY - rect.top,
    };
  }, []);

  useEffect(() => {
    const handlePointerMove = (e: PointerEvent) => {
      const drag = dragRef.current;
      if (!drag || e.pointerId !== drag.pointerId) {
        return;
      }
      e.preventDefault();
      const left = Math.max(
        0,
        Math.min(e.clientX - drag.offsetX, window.innerWidth - 100),
      );
      const top = Math.max(
        0,
        Math.min(e.clientY - drag.offsetY, window.innerHeight - 100),
      );
      setDragPosition({ left, top });
    };
    const stopDrag = (e: PointerEvent) => {
      const drag = dragRef.current;
      if (!drag || e.pointerId !== drag.pointerId) {
        return;
      }
      dragRef.current = null;
    };
    window.addEventListener("pointermove", handlePointerMove, {
      passive: false,
    });
    window.addEventListener("pointerup", stopDrag);
    window.addEventListener("pointercancel", stopDrag);
    return () => {
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", stopDrag);
      window.removeEventListener("pointercancel", stopDrag);
    };
  }, []);

  // Report the docked height so the touch canvas can inset above the keyboard.
  // Only the bottom-docked mobile panel covers the canvas — the desktop panel
  // floats and is draggable, so it reports 0. A ResizeObserver keeps the inset
  // in sync as the panel reflows (screen toggle, rotation), and the cleanup
  // clears it when the panel closes or switches to floating.
  useDockedHeight(panelRef, onDockedHeightChange);

  // Fire a key with the sticky modifiers held around it, then clear them —
  // sticky modifiers are one-shot, like a physical Shift you tap-then-release.
  // The remote resolves the shifted symbol from the held Shift, so a printable
  // "1" with Shift active correctly produces "!" on both RDP and VNC.
  const fireKeyWithModifiers = useCallback(
    (code: string) => {
      sendKeyCombo([...modifiers, code]);
      if (modifiers.size > 0) {
        setModifiers(new Set());
      }
    },
    [modifiers, sendKeyCombo],
  );

  const handleKeyPress = useCallback(
    (def: SoftKeyDefinition) => {
      if (def.type === "special" && modifierOf(def)) {
        const { code } = def;
        setModifiers((prev) => {
          const next = new Set(prev);
          if (!next.delete(code)) {
            next.add(code);
          }
          return next;
        });
        return;
      }
      if (def.type === "combo") {
        sendKeyCombo(def.codes);
        return;
      }
      // printable / non-modifier special: the DOM code, with any sticky
      // modifiers held around it.
      fireKeyWithModifiers(def.code);
    },
    [fireKeyWithModifiers, sendKeyCombo],
  );

  const handleKeyRelease = useCallback((_def: SoftKeyDefinition) => {
    // Presses are transient (down+up inside sendKeyCombo); pointer-up only
    // needs to stop key repeat, which SoftKeyButton handles itself.
  }, []);

  const topRow = screen === "primary" ? GUI_COMBO_ROW : FUNCTION_KEY_ROW;
  const shift = shiftHeld(modifiers);
  const mainRows =
    screen === "primary" ? PRIMARY_SCREEN_ROWS : SECONDARY_SCREEN_ROWS;

  const panelStyle = dragPosition
    ? {
        left: `${dragPosition.left}px`,
        top: `${dragPosition.top}px`,
        right: "auto",
        bottom: "auto",
      }
    : undefined;

  return (
    <div className="sk-panel" ref={panelRef} style={panelStyle}>
      {/* Desktop drag bar + close */}
      <div className="sk-toolbar">
        <div className="sk-toolbar-spacer" />
        <button
          type="button"
          className="sk-drag-handle"
          aria-label="Drag soft keyboard"
          onPointerDown={handleDragStart}
        >
          ⠿
        </button>
        <button
          type="button"
          className="sk-toolbar-close"
          aria-label="Close soft keyboard"
          onClick={() => {
            if (!dragRef.current) {
              onClose();
            }
          }}
        >
          ✕
        </button>
      </div>

      {isDesktop ? (
        <DesktopKeyboardGrid
          modifiers={modifiers}
          onPress={handleKeyPress}
          onRelease={handleKeyRelease}
        />
      ) : (
        <>
          {/* Top scrollable row: combos (primary) or function keys (secondary) */}
          <div
            className={screen === "primary" ? "sk-combo-row" : "sk-fkey-row"}
          >
            {topRow.map((def) => (
              <SoftKeyButton
                key={keyOf(def)}
                def={def}
                shift={shift}
                onPress={handleKeyPress}
                onRelease={handleKeyRelease}
                isActive={isHeld(def, modifiers)}
                scrollable
              />
            ))}
          </div>

          {/* Main rows */}
          <div className="sk-grid">
            {mainRows.map((row, rowIndex) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: stable row order
              <div key={rowIndex} className="sk-row">
                {row === ROW_HOME && <div className="sk-half-spacer" />}
                {row.map((def) => (
                  <SoftKeyButton
                    key={keyOf(def)}
                    def={def}
                    shift={shift}
                    onPress={handleKeyPress}
                    onRelease={handleKeyRelease}
                    isActive={isHeld(def, modifiers)}
                  />
                ))}
                {row === ROW_HOME && <div className="sk-half-spacer" />}
              </div>
            ))}
          </div>

          {/* Screen toggle + modifier indicators + close */}
          <div className="sk-status-row">
            <button
              type="button"
              className="sk-screen-toggle"
              onClick={() =>
                setScreen(screen === "primary" ? "secondary" : "primary")
              }
            >
              {screen === "primary" ? "Sym/Nav" : "ABC"}
            </button>
            <div className="sk-modifier-indicators">
              {[...modifiers].map((code) => (
                <span key={code} className="sk-modifier-badge">
                  {MODIFIER_KEYS.get(code)?.label ?? code}
                </span>
              ))}
            </div>
            <button
              type="button"
              className="sk-mobile-close"
              aria-label="Close soft keyboard"
              onClick={onClose}
            >
              ✕
            </button>
          </div>
        </>
      )}
    </div>
  );
}
