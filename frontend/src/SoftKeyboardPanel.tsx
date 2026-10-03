import {
  type CSSProperties,
  type RefObject,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useDockedHeight } from "./dockedPanel.ts";
import {
  type CellId,
  cellsOf,
  type LayoutCell,
  type LayoutPage,
  type LayoutRow,
  labelOf,
  MODIFIER_KEYS,
  PAGE_PC,
  PAGES,
  type PageId,
  type SoftKeyDefinition,
  shiftHeld,
} from "./softKeyboard.ts";
import {
  createHitTester,
  type GeometryRow,
  type Rect,
} from "./softKeyGeometry.ts";
import {
  createPressEngine,
  type HitTester,
  type ModifierState,
  type PointerKind,
  type PointerSample,
  type PressCommand,
  type PressEngine,
  type StepResult,
} from "./softKeyPress.ts";
import { TABLET_MIN_SHORT_SIDE } from "./tabletGuestSize.ts";
import { CAN_PINCH_ZOOM } from "./useRemoteDesktop.ts";

// The soft keyboard: a docked phone keyboard, or a floating PC grid for
// everything else. Both are pages of softKeyboard.ts rendered as cells, and
// one press engine (softKeyPress.ts) listening on the key area turns fingers
// into keys for both — no cell has a handler of its own.

// A phone is a touch device whose screen's short side is a phone's, in either
// orientation: the one client whose keyboard docks along the bottom and insets
// the canvas, because its screen cannot spare the room a floating card takes
// and its desktop pans (CAN_PINCH_ZOOM) rather than scrolls. Tablets and
// pointer clients, narrow windows included, get the floating grid.
function isPhone(): boolean {
  return (
    CAN_PINCH_ZOOM &&
    Math.min(screen.width, screen.height) < TABLET_MIN_SHORT_SIDE
  );
}

interface SoftKeyboardPanelProps {
  // Presses each DOM code in order then releases in reverse (transient — see
  // useRemoteDesktop.sendKeyCombo). The panel's only channel to the remote.
  sendKeyCombo: (codes: string[]) => void;
  onClose: () => void;
  // Reports the panel's height (CSS px) while it's docked to the bottom edge
  // (phone), 0 while it floats or when it unmounts. Lets the touch canvas pan
  // up above the keyboard instead of hiding under it.
  onDockedHeightChange?: (px: number) => void;
  // Hands focus back to the desktop surface when a key finds it lost: the
  // physical keyboard's listeners live there.
  onFocusDesktop: () => void;
}

// ── The engine's host: DOM events in, state and keys out ──

interface EngineHandlers {
  sendKeyCombo: (codes: string[]) => void;
  onFocusDesktop: () => void;
  setPage: (page: PageId) => void;
  setHeld: (held: ReadonlyMap<string, ModifierState>) => void;
  setActive: (ids: ReadonlySet<CellId>) => void;
  setPreviews: (
    update: (
      prev: ReadonlyMap<number, Preview>,
    ) => ReadonlyMap<number, Preview>,
  ) => void;
}

interface Preview {
  id: CellId;
  // The cell's centre x and top y, relative to the key area.
  x: number;
  y: number;
}

function toRect(r: DOMRect): Rect {
  return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
}

// Measure every key of the area into a hit tester. The shortcut row is left
// out: its keys commit on tap where they are touched and scroll under a slide,
// so a slide never resolves into it, and the area's bounds start below it.
function measure(area: HTMLElement): HitTester {
  const rows: GeometryRow[] = [];
  let top = area.getBoundingClientRect().top;
  for (const rowEl of area.querySelectorAll<HTMLElement>("[data-row]")) {
    if (rowEl.dataset.row === "shortcut") {
      top = Math.max(top, rowEl.getBoundingClientRect().bottom);
      continue;
    }
    const cells = [...rowEl.querySelectorAll<HTMLElement>("[data-cell]")].map(
      (el) => ({
        id: el.dataset.cell ?? "",
        rect: toRect(el.getBoundingClientRect()),
        spacer: el.dataset.spacer === "true",
      }),
    );
    if (cells.length > 0) {
      rows.push({ rect: toRect(rowEl.getBoundingClientRect()), cells });
    }
  }
  const bounds = { ...toRect(area.getBoundingClientRect()), top };
  return createHitTester(rows, bounds);
}

function pointerKind(type: string): PointerKind {
  return type === "touch" || type === "pen" ? type : "mouse";
}

// Where a cell sits, for the preview bubble over it.
function previewOf(area: HTMLElement, id: CellId): Preview | null {
  const el = area.querySelector<HTMLElement>(`[data-cell="${id}"]`);
  if (!el) {
    return null;
  }
  const a = area.getBoundingClientRect();
  const r = el.getBoundingClientRect();
  return { id, x: r.left - a.left + r.width / 2, y: r.top - a.top };
}

// The cell a pointer went down on, as the DOM has it, or `skip` for a control
// in the key area with a click of its own (the close button).
function cellUnder(e: PointerEvent): { cell: CellId | null; skip: boolean } {
  const target = e.target instanceof Element ? e.target : null;
  const cellEl = target?.closest<HTMLElement>("[data-cell]") ?? null;
  if (cellEl) {
    return { cell: cellEl.dataset.cell ?? null, skip: false };
  }
  return { cell: null, skip: target?.closest("button") !== null };
}

// A touch is captured by its target already; a mouse is not, and its release
// outside the panel would otherwise be lost.
function capture(area: HTMLElement, e: PointerEvent) {
  if (e.pointerType === "touch") {
    return;
  }
  try {
    area.setPointerCapture(e.pointerId);
  } catch {
    // A pointer that is already gone: its up or cancel follows.
  }
}

// A key is being typed; if focus has fallen to the body, the physical keyboard
// has gone silent with it.
function focusIfLost(h: EngineHandlers) {
  if (
    document.activeElement === null ||
    document.activeElement === document.body
  ) {
    h.onFocusDesktop();
  }
}

function withPreview(
  prev: ReadonlyMap<number, Preview>,
  pointerId: number,
  preview: Preview | null,
): ReadonlyMap<number, Preview> {
  const next = new Map(prev);
  if (preview) {
    next.set(pointerId, preview);
  } else {
    next.delete(pointerId);
  }
  return next;
}

// A tick of feedback where the device has it (Android); iOS has no such API.
function vibrate() {
  if (typeof navigator.vibrate === "function") {
    navigator.vibrate(10);
  }
}

// Keep the engine for the panel's life, listen on the key area, and run its
// commands. The handlers are read through a ref so the listeners are attached
// once per page and never go stale. Returns the call that forgets the keys'
// measured positions, for whatever moves them without changing the page (the
// floating panel's drag).
function useSoftKeyEngine(
  areaRef: RefObject<HTMLDivElement | null>,
  page: LayoutPage,
  handlers: EngineHandlers,
): () => void {
  const handlersRef = useRef(handlers);
  useLayoutEffect(() => {
    handlersRef.current = handlers;
  });

  const geometryRef = useRef<HitTester | null>(null);
  const engineRef = useRef<PressEngine | null>(null);
  if (engineRef.current === null) {
    engineRef.current = createPressEngine((x, y, prefer) => {
      if (geometryRef.current === null) {
        const area = areaRef.current;
        geometryRef.current = area ? measure(area) : () => null;
      }
      return geometryRef.current(x, y, prefer);
    }, cellsOf(page));
  }

  // The keys moved: measure again at the next touch.
  const invalidateGeometry = useCallback(() => {
    geometryRef.current = null;
  }, []);

  useLayoutEffect(() => {
    const area = areaRef.current;
    if (!area) {
      return;
    }
    const observer = new ResizeObserver(invalidateGeometry);
    observer.observe(area);
    window.addEventListener("resize", invalidateGeometry);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", invalidateGeometry);
    };
  }, [areaRef, invalidateGeometry]);

  useEffect(() => {
    const area = areaRef.current;
    const engine = engineRef.current;
    if (!area || !engine) {
      return;
    }
    let timer: number | null = null;

    const run = (command: PressCommand) => {
      const h = handlersRef.current;
      switch (command.kind) {
        case "send":
          h.sendKeyCombo(command.codes);
          focusIfLost(h);
          break;
        case "modifiers":
          h.setHeld(command.held);
          break;
        case "active":
          h.setActive(command.ids);
          break;
        case "preview": {
          const { pointerId, id } = command;
          const preview = id === null ? null : previewOf(area, id);
          h.setPreviews((prev) => withPreview(prev, pointerId, preview));
          break;
        }
        case "page":
          h.setPage(command.page);
          break;
        case "haptic":
          vibrate();
          break;
      }
    };

    const apply = (result: StepResult) => {
      for (const command of result.commands) {
        run(command);
      }
      if (timer !== null) {
        window.clearTimeout(timer);
        timer = null;
      }
      if (result.nextTickAt !== null) {
        const delay = Math.max(0, result.nextTickAt - performance.now());
        timer = window.setTimeout(() => {
          timer = null;
          apply(engine.handle({ kind: "tick", t: performance.now() }));
        }, delay);
      }
    };

    // A new page: new cells, and new positions under the next touch.
    geometryRef.current = null;
    apply(engine.handle({ kind: "layout", cells: cellsOf(page) }));

    const sample = (e: PointerEvent): PointerSample => ({
      id: e.pointerId,
      kind: pointerKind(e.pointerType),
      x: e.clientX,
      y: e.clientY,
      t: performance.now(),
    });
    const onDown = (e: PointerEvent) => {
      if (e.pointerType === "mouse" && e.button !== 0) {
        return;
      }
      const { cell, skip } = cellUnder(e);
      if (skip) {
        return;
      }
      // No focus change, no text selection, no compatibility mouse events.
      e.preventDefault();
      capture(area, e);
      apply(engine.handle({ kind: "down", p: sample(e), cell }));
    };
    const onMove = (e: PointerEvent) =>
      apply(engine.handle({ kind: "move", p: sample(e) }));
    const onUp = (e: PointerEvent) =>
      apply(engine.handle({ kind: "up", p: sample(e) }));
    const onCancel = (e: PointerEvent) =>
      apply(
        engine.handle({
          kind: "cancel",
          id: e.pointerId,
          t: performance.now(),
        }),
      );
    const cancelAll = () =>
      apply(engine.handle({ kind: "cancelAll", t: performance.now() }));
    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        cancelAll();
      }
    };
    // Android's long-press menu would otherwise cancel a held key; the
    // compatibility mousedown would otherwise move focus in WebKit.
    const swallow = (e: Event) => {
      const target = e.target instanceof Element ? e.target : null;
      if (target?.closest("[data-cell]")) {
        e.preventDefault();
      }
    };

    area.addEventListener("pointerdown", onDown);
    area.addEventListener("pointermove", onMove);
    area.addEventListener("pointerup", onUp);
    area.addEventListener("pointercancel", onCancel);
    area.addEventListener("lostpointercapture", onCancel);
    area.addEventListener("contextmenu", swallow);
    area.addEventListener("mousedown", swallow);
    window.addEventListener("blur", cancelAll);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      area.removeEventListener("pointerdown", onDown);
      area.removeEventListener("pointermove", onMove);
      area.removeEventListener("pointerup", onUp);
      area.removeEventListener("pointercancel", onCancel);
      area.removeEventListener("lostpointercapture", onCancel);
      area.removeEventListener("contextmenu", swallow);
      area.removeEventListener("mousedown", swallow);
      window.removeEventListener("blur", cancelAll);
      document.removeEventListener("visibilitychange", onVisibility);
      if (timer !== null) {
        window.clearTimeout(timer);
      }
      // Whatever was held when the page changed or the panel closed is over.
      apply(engine.handle({ kind: "cancelAll", t: performance.now() }));
    };
  }, [areaRef, page]);

  return invalidateGeometry;
}

// ── Cells ──

const ARROW_NAMES: ReadonlyMap<string, string> = new Map([
  ["ArrowLeft", "Left"],
  ["ArrowUp", "Up"],
  ["ArrowDown", "Down"],
  ["ArrowRight", "Right"],
]);

function nameOf(def: SoftKeyDefinition): string {
  if (def.type === "special") {
    return ARROW_NAMES.get(def.code) ?? def.label;
  }
  return labelOf(def, false);
}

interface CellProps {
  cell: LayoutCell;
  shift: boolean;
  active: boolean;
  modifier: ModifierState | undefined;
}

// A cell is the whole hit area, edge to edge with its neighbours; the keycap
// inside it is drawn inset and is only a picture. It has no handler of its
// own: the key area's engine decides what a touch on it means. A real button
// for the semantics and the accessible name, out of the tab order because the
// physical keyboard types on the remote, not on this one.
function Cell({ cell, shift, active, modifier }: CellProps) {
  const style = { "--u": cell.units } as CSSProperties;
  const { def } = cell;
  if (def.type === "spacer") {
    return (
      <div
        className="sk-cell sk-spacer"
        data-cell={cell.id}
        data-spacer="true"
        style={style}
      />
    );
  }
  const label = labelOf(def, shift);
  const hint =
    def.type === "printable" &&
    !shift &&
    !def.shifted &&
    def.shiftLabel &&
    def.shiftLabel !== def.label.toUpperCase()
      ? def.shiftLabel
      : null;
  const kind =
    def.type === "special" && MODIFIER_KEYS.has(def.code)
      ? "modifier"
      : def.type;
  return (
    <button
      type="button"
      tabIndex={-1}
      className={`sk-cell sk-${kind}`}
      aria-label={nameOf(def)}
      aria-pressed={kind === "modifier" ? modifier !== undefined : undefined}
      data-cell={cell.id}
      data-active={active ? "" : undefined}
      data-mod={modifier}
      style={style}
    >
      <span className={`sk-cap${label.length === 1 ? " sk-glyph" : ""}`}>
        {label}
        {hint && <span className="sk-hint">{hint}</span>}
      </span>
    </button>
  );
}

interface RowsProps {
  rows: LayoutRow[];
  shift: boolean;
  active: ReadonlySet<CellId>;
  held: ReadonlyMap<string, ModifierState>;
}

function modifierStateOf(
  cell: LayoutCell,
  held: ReadonlyMap<string, ModifierState>,
): ModifierState | undefined {
  return cell.def.type === "special" ? held.get(cell.def.code) : undefined;
}

function Rows({ rows, shift, active, held }: RowsProps) {
  return rows.map((row, index) => (
    <div
      // biome-ignore lint/suspicious/noArrayIndexKey: rows are a fixed order
      key={index}
      className={`sk-row sk-row-${row.kind}`}
      data-row={row.kind}
    >
      {row.cells.map((cell) => (
        <Cell
          key={cell.id}
          cell={cell}
          shift={shift}
          active={active.has(cell.id)}
          modifier={modifierStateOf(cell, held)}
        />
      ))}
    </div>
  ));
}

// ── The panel ──

export function SoftKeyboardPanel({
  sendKeyCombo,
  onClose,
  onDockedHeightChange,
  onFocusDesktop,
}: SoftKeyboardPanelProps) {
  const phone = useMemo(isPhone, []);
  const [pageId, setPageId] = useState<PageId>(phone ? "abc" : "pc");
  const page = PAGES.get(pageId) ?? PAGE_PC;
  const [held, setHeld] = useState<ReadonlyMap<string, ModifierState>>(
    () => new Map(),
  );
  const [active, setActive] = useState<ReadonlySet<CellId>>(() => new Set());
  const [previews, setPreviews] = useState<ReadonlyMap<number, Preview>>(
    () => new Map(),
  );

  const panelRef = useRef<HTMLDivElement>(null);
  const areaRef = useRef<HTMLDivElement>(null);

  const invalidateGeometry = useSoftKeyEngine(areaRef, page, {
    sendKeyCombo,
    onFocusDesktop,
    setPage: setPageId,
    setHeld,
    setActive,
    setPreviews,
  });

  // ── Drag (floating) ──
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
      // The keys have moved; the geometry under the next touch is new.
      invalidateGeometry();
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
  }, [invalidateGeometry]);

  useDockedHeight(panelRef, phone, onDockedHeightChange);

  const cells = useMemo(() => cellsOf(page), [page]);
  const shift = shiftHeld(held.keys());

  // Held modifiers with no key on this page — a right Alt locked on the Sym
  // page, say — are named beside the close button so they are never invisible.
  const unseen = useMemo(() => {
    const onPage = new Set<string>();
    for (const cell of cells.values()) {
      if (cell.def.type === "special") {
        onPage.add(cell.def.code);
      }
    }
    return [...held].filter(([code]) => !onPage.has(code));
  }, [cells, held]);

  const panelStyle = dragPosition
    ? {
        left: `${dragPosition.left}px`,
        top: `${dragPosition.top}px`,
        right: "auto",
        bottom: "auto",
      }
    : undefined;

  const shortcutRows = page.rows.filter((row) => row.kind === "shortcut");
  const keyRows = page.rows.filter((row) => row.kind !== "shortcut");

  return (
    <div
      className={`sk-panel ${phone ? "sk-docked" : "sk-floating"}`}
      ref={panelRef}
      style={panelStyle}
    >
      {!phone && (
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
            className="sk-close"
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
      )}

      <div className="sk-area" ref={areaRef}>
        {shortcutRows.map((row) => (
          <div key={row.kind} className="sk-shortcut" data-row="shortcut">
            <div className="sk-scroller">
              {row.cells.map((cell) => (
                <Cell
                  key={cell.id}
                  cell={cell}
                  shift={shift}
                  active={active.has(cell.id)}
                  modifier={modifierStateOf(cell, held)}
                />
              ))}
            </div>
            {unseen.length > 0 && (
              <div className="sk-badges">
                {unseen.map(([code, state]) => (
                  <span key={code} className="sk-badge" data-mod={state}>
                    {MODIFIER_KEYS.get(code)?.label ?? code}
                  </span>
                ))}
              </div>
            )}
            <button
              type="button"
              className="sk-close"
              aria-label="Close soft keyboard"
              onClick={onClose}
            >
              ✕
            </button>
          </div>
        ))}

        {page.side.length > 0 ? (
          <div className="sk-pc">
            <div className="sk-pc-main">
              <Rows rows={keyRows} shift={shift} active={active} held={held} />
            </div>
            <div className="sk-pc-side">
              <Rows
                rows={page.side}
                shift={shift}
                active={active}
                held={held}
              />
            </div>
          </div>
        ) : (
          <Rows rows={keyRows} shift={shift} active={active} held={held} />
        )}

        {[...previews.values()].map((preview) => {
          const cell = cells.get(preview.id);
          if (!cell) {
            return null;
          }
          return (
            <div
              key={preview.id}
              className="sk-preview"
              aria-hidden="true"
              style={{ left: `${preview.x}px`, top: `${preview.y}px` }}
            >
              {labelOf(cell.def, shift)}
            </div>
          );
        })}
      </div>
    </div>
  );
}
