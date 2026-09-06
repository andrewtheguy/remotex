// Soft-keyboard layout, expressed in DOM `KeyboardEvent.code` strings — the
// same currency the whole input path already speaks (see protocol.ts and the
// backend keymap.rs). The backend maps every DOM code to an RDP scancode *and*
// an X11 keysym, resolving the shifted symbol from the live Shift state (RDP
// defers to the remote host; VNC picks the shifted keysym in keymap.rs) — so a
// soft key is just a code, and shifted symbols fall out of holding the real
// Shift modifier. That reuses the existing pipeline for both engines instead
// of minting a second, keysym-only input path.

// ── Types ──

export interface PrintableSoftKey {
  type: "printable";
  label: string;
  code: string;
  // Cosmetic only: the glyph shown when Shift is active (e.g. "!" over "1").
  // Letters omit it — the display just upper-cases the label. The character
  // itself is produced by the remote from the held Shift, not from this field.
  shiftLabel?: string;
  width?: number;
}

export interface SpecialSoftKey {
  type: "special";
  label: string;
  code: string;
  width?: number;
}

export interface ComboSoftKey {
  type: "combo";
  label: string;
  // DOM codes pressed in order, released in reverse (see sendKeyCombo).
  codes: string[];
  width?: number;
}

export type SoftKeyDefinition =
  | PrintableSoftKey
  | SpecialSoftKey
  | ComboSoftKey;

// The sticky modifiers the panel can hold, one entry per physical key: both
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

// The modifier codes currently held sticky, in the order they were toggled on.
// Sent down in that order ahead of the key and released after it.
export type SoftKeyModifiers = ReadonlySet<string>;

// Which sticky modifier a key toggles, or null if it is not a modifier key. A
// modifier inside a combo (Ctrl in Ctrl+C) is not one: only a `special` key
// whose own code is a modifier is.
export function modifierOf(def: SoftKeyDefinition): ModifierKey | null {
  if (def.type !== "special") {
    return null;
  }
  return MODIFIER_KEYS.get(def.code) ?? null;
}

// Whether either Shift is held — what decides the glyphs the keys display.
export function shiftHeld(modifiers: SoftKeyModifiers): boolean {
  return modifiers.has("ShiftLeft") || modifiers.has("ShiftRight");
}

export type SoftKeyboardScreen = "primary" | "secondary";

// ── Builders ──

function p(
  label: string,
  code: string,
  shiftLabel?: string,
  width?: number,
): PrintableSoftKey {
  return { type: "printable", label, code, shiftLabel, width };
}

function s(label: string, code: string, width?: number): SpecialSoftKey {
  return { type: "special", label, code, width };
}

function c(label: string, codes: string[], width?: number): ComboSoftKey {
  return { type: "combo", label, codes, width };
}

// ── GUI combo row (scrollable quick-access, primary screen) ──

export const GUI_COMBO_ROW: SoftKeyDefinition[] = [
  s("Esc", "Escape"),
  c("Alt+Tab", ["AltLeft", "Tab"]),
  c("Alt+F4", ["AltLeft", "F4"]),
  c("C+A+Del", ["ControlLeft", "AltLeft", "Delete"]),
  s("Super", "MetaLeft"),
  c("Ctrl+Esc", ["ControlLeft", "Escape"]),
  c("Ctrl+Z", ["ControlLeft", "KeyZ"]),
  c("Ctrl+C", ["ControlLeft", "KeyC"]),
  c("Ctrl+V", ["ControlLeft", "KeyV"]),
  c("Ctrl+A", ["ControlLeft", "KeyA"]),
  c("Ctrl+S", ["ControlLeft", "KeyS"]),
];

// ── Primary screen: QWERTY ──

const ROW_DIGITS: SoftKeyDefinition[] = [
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

const ROW_QWERTY: SoftKeyDefinition[] = [
  p("q", "KeyQ"),
  p("w", "KeyW"),
  p("e", "KeyE"),
  p("r", "KeyR"),
  p("t", "KeyT"),
  p("y", "KeyY"),
  p("u", "KeyU"),
  p("i", "KeyI"),
  p("o", "KeyO"),
  p("p", "KeyP"),
];

// Exported so the panel can recognise it by identity and bracket it with half-key
// spacers. Nine keys where the rows around it have ten units, so without them
// `a`–`l` stretch to the full width, come out wider than the keys above, and stop
// lining up with the row they are meant to sit under. Identity rather than a row
// index, so inserting a row above cannot silently indent the wrong one.
export const ROW_HOME: SoftKeyDefinition[] = [
  p("a", "KeyA"),
  p("s", "KeyS"),
  p("d", "KeyD"),
  p("f", "KeyF"),
  p("g", "KeyG"),
  p("h", "KeyH"),
  p("j", "KeyJ"),
  p("k", "KeyK"),
  p("l", "KeyL"),
];

const ROW_ZXCV: SoftKeyDefinition[] = [
  s("Shift", "ShiftLeft", 1.5),
  p("z", "KeyZ"),
  p("x", "KeyX"),
  p("c", "KeyC"),
  p("v", "KeyV"),
  p("b", "KeyB"),
  p("n", "KeyN"),
  p("m", "KeyM"),
  s("Bksp", "Backspace", 1.5),
];

const ROW_BOTTOM: SoftKeyDefinition[] = [
  s("Tab", "Tab", 1.3),
  s("Ctrl", "ControlLeft", 1.3),
  s("Alt", "AltLeft", 1.3),
  s("Space", "Space", 3),
  s("Enter", "Enter", 2),
];

export const PRIMARY_SCREEN_ROWS: SoftKeyDefinition[][] = [
  ROW_DIGITS,
  ROW_QWERTY,
  ROW_HOME,
  ROW_ZXCV,
  ROW_BOTTOM,
];

// ── Secondary screen: symbols + navigation ──

const ROW_SYMBOLS_1: SoftKeyDefinition[] = [
  p("`", "Backquote", "~"),
  p("-", "Minus", "_"),
  p("=", "Equal", "+"),
  p("[", "BracketLeft", "{"),
  p("]", "BracketRight", "}"),
  p("\\", "Backslash", "|"),
  p(";", "Semicolon", ":"),
  p("'", "Quote", '"'),
  p(",", "Comma", "<"),
  p(".", "Period", ">"),
];

const ROW_SYMBOLS_2: SoftKeyDefinition[] = [
  p("/", "Slash", "?"),
  s("Ins", "Insert"),
  s("Del", "Delete"),
  s("Home", "Home"),
  s("End", "End"),
  s("PgUp", "PageUp"),
  s("PgDn", "PageDown"),
];

const ROW_NAV_ARROWS: SoftKeyDefinition[] = [
  s("←", "ArrowLeft", 1.5),
  s("↑", "ArrowUp", 1.5),
  s("↓", "ArrowDown", 1.5),
  s("→", "ArrowRight", 1.5),
];

// The Sym/Nav screen gets its own bottom row, and it carries the *right-hand*
// modifiers: the ABC screen's Shift, Ctrl and Alt (and the combo row's Super)
// are the left keys, so this is where the right ones live — the only way a
// phone reaches AltGr on a Windows or Linux host, or a Mac's right Option, and
// the Shift here also makes the shifted symbol glyphs (~ _ + { } | : " < > ?)
// reachable without switching screens. Space/Enter shrink to make room.
const ROW_BOTTOM_SECONDARY: SoftKeyDefinition[] = [
  s("RShift", "ShiftRight", 1.5),
  s("Tab", "Tab", 1.3),
  s("RCtrl", "ControlRight", 1.3),
  s("RAlt", "AltRight", 1.3),
  s("RSuper", "MetaRight", 1.3),
  s("Space", "Space", 2),
  s("Enter", "Enter", 2),
];

export const SECONDARY_SCREEN_ROWS: SoftKeyDefinition[][] = [
  ROW_SYMBOLS_1,
  ROW_SYMBOLS_2,
  ROW_NAV_ARROWS,
  ROW_BOTTOM_SECONDARY,
];

// ── Function key row (scrollable quick-access, secondary screen) ──

export const FUNCTION_KEY_ROW: SoftKeyDefinition[] = [
  s("F1", "F1"),
  s("F2", "F2"),
  s("F3", "F3"),
  s("F4", "F4"),
  s("F5", "F5"),
  s("F6", "F6"),
  s("F7", "F7"),
  s("F8", "F8"),
  s("F9", "F9"),
  s("F10", "F10"),
  s("F11", "F11"),
  s("F12", "F12"),
];

// ── Desktop PC keyboard layout (wide viewports) ──

export const DESKTOP_FUNCTION_ROW: SoftKeyDefinition[] = [
  s("Esc", "Escape"),
  s("F1", "F1"),
  s("F2", "F2"),
  s("F3", "F3"),
  s("F4", "F4"),
  s("F5", "F5"),
  s("F6", "F6"),
  s("F7", "F7"),
  s("F8", "F8"),
  s("F9", "F9"),
  s("F10", "F10"),
  s("F11", "F11"),
  s("F12", "F12"),
];

export const DESKTOP_NUMBER_ROW: SoftKeyDefinition[] = [
  p("`", "Backquote", "~"),
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
  p("-", "Minus", "_"),
  p("=", "Equal", "+"),
  s("Bksp", "Backspace"),
];

export const DESKTOP_QWERTY_ROW: SoftKeyDefinition[] = [
  s("Tab", "Tab"),
  p("q", "KeyQ"),
  p("w", "KeyW"),
  p("e", "KeyE"),
  p("r", "KeyR"),
  p("t", "KeyT"),
  p("y", "KeyY"),
  p("u", "KeyU"),
  p("i", "KeyI"),
  p("o", "KeyO"),
  p("p", "KeyP"),
  p("[", "BracketLeft", "{"),
  p("]", "BracketRight", "}"),
  p("\\", "Backslash", "|"),
];

export const DESKTOP_HOME_ROW: SoftKeyDefinition[] = [
  p("a", "KeyA"),
  p("s", "KeyS"),
  p("d", "KeyD"),
  p("f", "KeyF"),
  p("g", "KeyG"),
  p("h", "KeyH"),
  p("j", "KeyJ"),
  p("k", "KeyK"),
  p("l", "KeyL"),
  p(";", "Semicolon", ":"),
  p("'", "Quote", '"'),
  s("Enter", "Enter"),
];

export const DESKTOP_ZXCV_ROW: SoftKeyDefinition[] = [
  p("z", "KeyZ"),
  p("x", "KeyX"),
  p("c", "KeyC"),
  p("v", "KeyV"),
  p("b", "KeyB"),
  p("n", "KeyN"),
  p("m", "KeyM"),
  p(",", "Comma", "<"),
  p(".", "Period", ">"),
  p("/", "Slash", "?"),
];

export const DESKTOP_SPACE_KEY: SoftKeyDefinition = s("Space", "Space");

export const DESKTOP_NAV_ROW_1: SoftKeyDefinition[] = [
  s("Ins", "Insert"),
  s("Home", "Home"),
  s("PgUp", "PageUp"),
];

export const DESKTOP_NAV_ROW_2: SoftKeyDefinition[] = [
  s("Del", "Delete"),
  s("End", "End"),
  s("PgDn", "PageDown"),
];

export const DESKTOP_ARROW_ROW_1: SoftKeyDefinition[] = [s("▲", "ArrowUp")];

export const DESKTOP_ARROW_ROW_2: SoftKeyDefinition[] = [
  s("◀", "ArrowLeft"),
  s("▼", "ArrowDown"),
  s("▶", "ArrowRight"),
];

// The bottom row of a PC keyboard, as the hardware has it: Ctrl, Super and Alt
// left of the space bar, Alt, Super and Ctrl right of it, each with its own
// side's code. Labels match the keycaps rather than naming the side — the
// position says it, as on the physical board.
export const DESKTOP_SHIFT_LEFT: SpecialSoftKey = s("Shift", "ShiftLeft");
export const DESKTOP_SHIFT_RIGHT: SpecialSoftKey = s("Shift", "ShiftRight");

export const DESKTOP_BOTTOM_LEFT: SpecialSoftKey[] = [
  s("Ctrl", "ControlLeft"),
  s("Super", "MetaLeft"),
  s("Alt", "AltLeft"),
];

export const DESKTOP_BOTTOM_RIGHT: SpecialSoftKey[] = [
  s("Alt", "AltRight"),
  s("Super", "MetaRight"),
  s("Ctrl", "ControlRight"),
];
