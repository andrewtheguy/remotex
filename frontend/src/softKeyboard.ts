// Soft-keyboard layout, expressed in DOM `KeyboardEvent.code` strings — the
// same currency the whole input path already speaks (see protocol.ts and the
// backend keymap.rs). The backend maps every DOM code to an RDP scancode *and*
// an X11 keysym, resolving the shifted symbol from the live Shift state (RDP
// defers to the remote host; VNC picks the shifted keysym in keymap.rs) — so a
// soft key is just a code, and shifted symbols fall out of holding the real
// Shift modifier. That reuses the existing pipeline for both engines instead
// of minting a second, keysym-only input path.
//
// This module is data only: pages of rows of cells, each cell a key definition,
// a width in units and the way it commits. How a finger turns a cell into a
// key is softKeyPress.ts; where a finger is, softKeyGeometry.ts.

// ── Key definitions ──

export interface PrintableSoftKey {
  type: "printable";
  label: string;
  code: string;
  // Cosmetic only: the glyph shown in the corner for Shift (e.g. "!" over "1").
  // The character itself is produced by the remote from a held Shift.
  shiftLabel?: string;
  // The key *is* the shifted symbol: sent as Shift plus the code, so `_` or `{`
  // on the Sym page needs no Shift of its own.
  shifted?: boolean;
}

export interface SpecialSoftKey {
  type: "special";
  label: string;
  code: string;
}

export interface ComboSoftKey {
  type: "combo";
  label: string;
  // DOM codes pressed in order, released in reverse (see sendKeyCombo).
  codes: string[];
}

// Switches the phone keyboard to another page.
export interface PageSoftKey {
  type: "page";
  label: string;
  page: PageId;
}

// Switches the modifiers between sticking and being sent alone on a tap. Sits
// in the PC grid's Caps Lock slot, and fixed ahead of the shortcut row on both
// phone pages.
export interface StickySoftKey {
  type: "sticky";
  label: string;
}

// Inert width: the half key at each end of the home row. Drawn as nothing; a
// finger on it belongs to the neighbouring key.
export interface SpacerSoftKey {
  type: "spacer";
}

export type SoftKeyDefinition =
  | PrintableSoftKey
  | SpecialSoftKey
  | ComboSoftKey
  | PageSoftKey
  | StickySoftKey
  | SpacerSoftKey;

// ── Modifiers ──

// The modifiers the keyboard can hold, one entry per physical key: both
// sides of Shift, Ctrl, Alt and Super are distinct codes, exactly as a hardware
// keyboard reports them, and the backend keeps them apart (Alt_R vs Alt_L, the
// E0-extended scancode) — so a right-hand soft key really is the right-hand key
// on the remote, where the two differ (AltGr, a Mac's right Option).
export type ModifierKind = "shift" | "ctrl" | "alt" | "super";
export type ModifierSide = "left" | "right";

export interface ModifierKey {
  kind: ModifierKind;
  side: ModifierSide;
  // The name a modifier badge shows for it while held.
  label: string;
}

export const MODIFIER_KEYS: ReadonlyMap<string, ModifierKey> = new Map([
  ["ShiftLeft", { kind: "shift", side: "left", label: "Shift" }],
  ["ShiftRight", { kind: "shift", side: "right", label: "RShift" }],
  ["ControlLeft", { kind: "ctrl", side: "left", label: "Ctrl" }],
  ["ControlRight", { kind: "ctrl", side: "right", label: "RCtrl" }],
  ["AltLeft", { kind: "alt", side: "left", label: "Alt" }],
  ["AltRight", { kind: "alt", side: "right", label: "RAlt" }],
  ["MetaLeft", { kind: "super", side: "left", label: "Super" }],
  ["MetaRight", { kind: "super", side: "right", label: "RSuper" }],
]);

// Which modifier a key holds, or null if it is not a modifier key. A
// modifier inside a combo (Ctrl in Ctrl+C) is not one: only a `special` key
// whose own code is a modifier is.
export function modifierOf(def: SoftKeyDefinition): ModifierKey | null {
  if (def.type !== "special") {
    return null;
  }
  return MODIFIER_KEYS.get(def.code) ?? null;
}

// Whether either Shift is among the held codes — what decides the glyphs the
// keys display.
export function shiftHeld(held: Iterable<string>): boolean {
  for (const code of held) {
    if (code === "ShiftLeft" || code === "ShiftRight") {
      return true;
    }
  }
  return false;
}

// ── Layout model ──

export type PageId = "abc" | "sym" | "pc";

// How a cell turns a finger into a key:
// - `lift`: the key under the finger when it lifts, after any slide to correct;
// - `down`: at once on touch, then repeating while held (Backspace, arrows);
// - `tap`: on lift, only if the finger stayed put — the scrollable shortcut row,
//   where a slide is the row scrolling and never a change of key.
export type Commit = "lift" | "down" | "tap";

// A cell's identity within its page: "page:row:col". Never a code — Tab sits in
// two places on a PC keyboard and both Shifts share a label.
export type CellId = string;

export interface LayoutCell {
  id: CellId;
  def: SoftKeyDefinition;
  // Width as a share of the row: a phone row is ten units.
  units: number;
  commit: Commit;
}

export type RowKind = "shortcut" | "strip" | "main" | "side";

export interface LayoutRow {
  kind: RowKind;
  cells: LayoutCell[];
}

export interface LayoutPage {
  id: PageId;
  rows: LayoutRow[];
  // The PC grid's navigation and arrow cluster, laid beside `rows`.
  side: LayoutRow[];
}

// The keys that commit on touch and repeat while held: the editing and cursor
// keys a physical keyboard's typematic serves, and nothing that types a
// character a slide could still correct. Enter is deliberately absent — an
// accidental Enter in a terminal is the costliest mis-hit on this surface.
export const REPEATING_CODES: ReadonlySet<string> = new Set([
  "Backspace",
  "Delete",
  "ArrowLeft",
  "ArrowRight",
  "ArrowUp",
  "ArrowDown",
  "Space",
  "Tab",
  "PageUp",
  "PageDown",
]);

function commitOf(def: SoftKeyDefinition): Commit {
  if (def.type === "special" && REPEATING_CODES.has(def.code)) {
    return "down";
  }
  return "lift";
}

// ── Builders ──

interface Key {
  def: SoftKeyDefinition;
  units: number;
}

function p(label: string, code: string, shiftLabel?: string, units = 1): Key {
  return { def: { type: "printable", label, code, shiftLabel }, units };
}

// A shifted printable: the key is the shifted symbol itself.
function ps(label: string, code: string, units = 1): Key {
  return { def: { type: "printable", label, code, shifted: true }, units };
}

function s(label: string, code: string, units = 1): Key {
  return { def: { type: "special", label, code }, units };
}

function c(label: string, codes: string[], units = 1): Key {
  return { def: { type: "combo", label, codes }, units };
}

function pg(label: string, page: PageId, units = 1): Key {
  return { def: { type: "page", label, page }, units };
}

function sticky(units = 1): Key {
  return { def: { type: "sticky", label: "Sticky" }, units };
}

function gap(units: number): Key {
  return { def: { type: "spacer" }, units };
}

function row(
  page: PageId,
  index: number,
  kind: RowKind,
  keys: Key[],
  commit?: Commit,
): LayoutRow {
  return {
    kind,
    cells: keys.map((key, col) => ({
      id: `${page}:${index}:${col}`,
      def: key.def,
      units: key.units,
      commit: commit ?? commitOf(key.def),
    })),
  };
}

function page(
  id: PageId,
  rows: { kind: RowKind; keys: Key[]; commit?: Commit }[],
  side: { kind: RowKind; keys: Key[] }[] = [],
): LayoutPage {
  const main = rows.map((r, i) => row(id, i, r.kind, r.keys, r.commit));
  const cluster = side.map((r, i) => row(id, rows.length + i, r.kind, r.keys));
  return { id, rows: main, side: cluster };
}

// ── Shortcut rows (scrollable, phone pages) ──

// The Sticky key leads both rows, and the panel keeps it out of the scroll so
// it is on screen on either page. With it off, a tap on a modifier sends that
// key alone, down then up — Super alone is the Start key.
//
// The chords are the ones a browser swallows or a phone cannot otherwise
// reach; Ctrl+C and its kin are the strip's Ctrl and a letter.
const SHORTCUTS_ABC: Key[] = [
  sticky(),
  c("Alt+Tab", ["AltLeft", "Tab"]),
  c("Alt+F4", ["AltLeft", "F4"]),
  c("C+A+Del", ["ControlLeft", "AltLeft", "Delete"]),
];

// The Sym page's row is F1 to F12.
const SHORTCUTS_FN: Key[] = [
  sticky(),
  ...Array.from({ length: 12 }, (_, i) => s(`F${i + 1}`, `F${i + 1}`)),
];

// ── The strip: modifiers, Esc and arrows, on every phone page ──

// Esc sits beside Tab, ahead of the modifiers. Super is a symbol where the word
// would not fit, and all of them are kept narrow so the arrows keep a unit each.
const SUPER_GLYPH = "❖";

const STRIP: Key[] = [
  s("Tab", "Tab", 1.5),
  s("Esc", "Escape", 1.1),
  s("Ctrl", "ControlLeft", 1.1),
  s("Alt", "AltLeft", 1.1),
  s(SUPER_GLYPH, "MetaLeft", 1.2),
  s("←", "ArrowLeft"),
  s("↑", "ArrowUp"),
  s("↓", "ArrowDown"),
  s("→", "ArrowRight"),
];

// ── Phone: ABC page ──

const DIGITS: Key[] = [
  p("1", "Digit1", "!"),
  p("2", "Digit2", "@"),
  p("3", "Digit3", "#"),
  p("4", "Digit4", "$"),
  p("5", "Digit5", "%"),
  p("6", "Digit6", "^"),
  p("7", "Digit7", "&"),
  p("8", "Digit8", "*"),
  p("9", "Digit9", "("),
  p("0", "Digit0", ")"),
];

const letters = (keys: string): Key[] =>
  [...keys].map((k) => p(k, `Key${k.toUpperCase()}`));

const QWERTY: Key[] = letters("qwertyuiop");

// Nine keys where the rows around it have ten units: half a key of inert
// margin at each end keeps the stagger of a real keyboard and the key width of
// the rows above. A finger on the margin goes to `a` or `l`.
const HOME: Key[] = [gap(0.5), ...letters("asdfghjkl"), gap(0.5)];

const ZXCV: Key[] = [
  s("Shift", "ShiftLeft", 1.5),
  ...letters("zxcvbnm"),
  s("Bksp", "Backspace", 1.5),
];

const BOTTOM_ABC: Key[] = [
  pg("?123", "sym", 1.5),
  p(",", "Comma", "<"),
  s("Space", "Space", 5),
  p(".", "Period", ">"),
  s("Enter", "Enter", 1.5),
];

// ── Phone: Sym/Nav page ──

const SYMBOLS: Key[] = [
  p("`", "Backquote"),
  p("-", "Minus"),
  p("=", "Equal"),
  p("[", "BracketLeft"),
  p("]", "BracketRight"),
  p("\\", "Backslash"),
  p(";", "Semicolon"),
  p("'", "Quote"),
  p(",", "Comma"),
  p(".", "Period"),
];

// The shifted partners of the row above, each a key of its own.
const SYMBOLS_SHIFTED: Key[] = [
  ps("~", "Backquote"),
  ps("_", "Minus"),
  ps("+", "Equal"),
  ps("{", "BracketLeft"),
  ps("}", "BracketRight"),
  ps("|", "Backslash"),
  ps(":", "Semicolon"),
  ps('"', "Quote"),
  ps("<", "Comma"),
  ps(">", "Period"),
];

const NAV: Key[] = [
  p("/", "Slash"),
  ps("?", "Slash"),
  s("Ins", "Insert"),
  s("Del", "Delete"),
  s("Home", "Home"),
  s("End", "End"),
  s("PgUp", "PageUp"),
  s("PgDn", "PageDown"),
  s("PrtSc", "PrintScreen"),
  s("Menu", "ContextMenu"),
];

// The right-hand modifiers live here and nowhere else on a phone: the ABC
// page's Shift, Ctrl, Alt and Super are the left keys, so this is the only way
// a phone reaches AltGr on a Windows or Linux host, or a Mac's right Option.
// Backspace ends the row, where the ABC page has it, as wide as each of them.
const RIGHT_MODIFIERS: Key[] = [
  s("RShift", "ShiftRight", 2),
  s("RCtrl", "ControlRight", 2),
  s("RAlt", "AltRight", 2),
  s("RSuper", "MetaRight", 2),
  s("Bksp", "Backspace", 2),
];

const BOTTOM_SYM: Key[] = [
  pg("ABC", "abc", 1.5),
  p(",", "Comma", "<"),
  s("Space", "Space", 5),
  p(".", "Period", ">"),
  s("Enter", "Enter", 1.5),
];

// ── PC grid (floating, wide and non-phone clients) ──

const PC_FUNCTION_ROW: Key[] = [
  s("Esc", "Escape", 1.2),
  ...Array.from({ length: 12 }, (_, i) => s(`F${i + 1}`, `F${i + 1}`)),
];

const PC_NUMBER_ROW: Key[] = [
  p("`", "Backquote", "~"),
  ...DIGITS,
  p("-", "Minus", "_"),
  p("=", "Equal", "+"),
  s("Bksp", "Backspace", 1.75),
];

const PC_QWERTY_ROW: Key[] = [
  s("Tab", "Tab", 1.45),
  ...QWERTY,
  p("[", "BracketLeft", "{"),
  p("]", "BracketRight", "}"),
  p("\\", "Backslash", "|", 1.15),
];

const PC_HOME_ROW: Key[] = [
  sticky(2.1),
  ...letters("asdfghjkl"),
  p(";", "Semicolon", ":"),
  p("'", "Quote", '"'),
  s("Enter", "Enter", 1.95),
];

const PC_ZXCV_ROW: Key[] = [
  s("Shift", "ShiftLeft", 1.95),
  ...letters("zxcvbnm"),
  p(",", "Comma", "<"),
  p(".", "Period", ">"),
  p("/", "Slash", "?"),
  s("Shift", "ShiftRight", 1.95),
];

// The bottom row of a PC keyboard, as the hardware has it: Ctrl, Super and Alt
// left of the space bar, Alt, Super and Ctrl right of it, each with its own
// side's code. Labels match the keycaps rather than naming the side — the
// position says it, as on the physical board.
const PC_BOTTOM_ROW: Key[] = [
  s("Ctrl", "ControlLeft", 1.1),
  s("Super", "MetaLeft", 1.1),
  s("Alt", "AltLeft", 1.1),
  s("Space", "Space", 9),
  s("Alt", "AltRight", 1.1),
  s("Super", "MetaRight", 1.1),
  s("Ctrl", "ControlRight", 1.1),
];

const PC_SIDE: { kind: RowKind; keys: Key[] }[] = [
  {
    kind: "side",
    keys: [s("Ins", "Insert"), s("Home", "Home"), s("PgUp", "PageUp")],
  },
  {
    kind: "side",
    keys: [s("Del", "Delete"), s("End", "End"), s("PgDn", "PageDown")],
  },
  { kind: "side", keys: [gap(1), s("▲", "ArrowUp"), gap(1)] },
  {
    kind: "side",
    keys: [s("◀", "ArrowLeft"), s("▼", "ArrowDown"), s("▶", "ArrowRight")],
  },
];

// ── Pages ──

export const PAGE_ABC: LayoutPage = page("abc", [
  { kind: "shortcut", keys: SHORTCUTS_ABC, commit: "tap" },
  { kind: "strip", keys: STRIP },
  { kind: "main", keys: DIGITS },
  { kind: "main", keys: QWERTY },
  { kind: "main", keys: HOME },
  { kind: "main", keys: ZXCV },
  { kind: "main", keys: BOTTOM_ABC },
]);

export const PAGE_SYM: LayoutPage = page("sym", [
  { kind: "shortcut", keys: SHORTCUTS_FN, commit: "tap" },
  { kind: "strip", keys: STRIP },
  { kind: "main", keys: SYMBOLS },
  { kind: "main", keys: SYMBOLS_SHIFTED },
  { kind: "main", keys: NAV },
  { kind: "main", keys: RIGHT_MODIFIERS },
  { kind: "main", keys: BOTTOM_SYM },
]);

export const PAGE_PC: LayoutPage = page(
  "pc",
  [
    { kind: "main", keys: PC_FUNCTION_ROW },
    { kind: "main", keys: PC_NUMBER_ROW },
    { kind: "main", keys: PC_QWERTY_ROW },
    { kind: "main", keys: PC_HOME_ROW },
    { kind: "main", keys: PC_ZXCV_ROW },
    { kind: "main", keys: PC_BOTTOM_ROW },
  ],
  PC_SIDE,
);

export const PAGES: ReadonlyMap<PageId, LayoutPage> = new Map([
  ["abc", PAGE_ABC],
  ["sym", PAGE_SYM],
  ["pc", PAGE_PC],
]);

// Every cell of a page by id, main rows and side cluster alike — what the
// press engine is handed on a page switch.
export function cellsOf(page: LayoutPage): ReadonlyMap<CellId, LayoutCell> {
  const cells = new Map<CellId, LayoutCell>();
  for (const r of [...page.rows, ...page.side]) {
    for (const cell of r.cells) {
      cells.set(cell.id, cell);
    }
  }
  return cells;
}

// The label a cell shows: a printable's shifted glyph while a Shift is held.
export function labelOf(def: SoftKeyDefinition, shift: boolean): string {
  switch (def.type) {
    case "spacer":
      return "";
    case "printable":
      if (shift && !def.shifted) {
        return def.shiftLabel ?? def.label.toUpperCase();
      }
      return def.label;
    default:
      return def.label;
  }
}
