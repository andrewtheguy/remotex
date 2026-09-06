// Where each side's modifiers live on the soft keyboard. The phone layout puts
// the left keys on the ABC screen and the right keys on Sym/Nav; the desktop
// grid follows a PC keyboard, left group before the space bar and right group
// after it. Run with `bun test src/softKeyboard.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  DESKTOP_BOTTOM_LEFT,
  DESKTOP_BOTTOM_RIGHT,
  DESKTOP_SHIFT_LEFT,
  DESKTOP_SHIFT_RIGHT,
  GUI_COMBO_ROW,
  MODIFIER_KEYS,
  modifierOf,
  PRIMARY_SCREEN_ROWS,
  SECONDARY_SCREEN_ROWS,
  type SoftKeyDefinition,
  shiftHeld,
} from "./softKeyboard.ts";

const sidesOf = (defs: SoftKeyDefinition[]) =>
  new Set(
    defs
      .map((def) => modifierOf(def)?.side)
      .filter((side) => side !== undefined),
  );

test("the ABC screen and its combo row hold only left-hand modifiers", () => {
  const sides = sidesOf([...GUI_COMBO_ROW, ...PRIMARY_SCREEN_ROWS.flat()]);
  assert.deepEqual([...sides], ["left"]);
});

test("the Sym/Nav screen holds every right-hand modifier and no left one", () => {
  const defs = SECONDARY_SCREEN_ROWS.flat();
  assert.deepEqual([...sidesOf(defs)], ["right"]);
  const codes = new Set(
    defs.filter((def) => def.type === "special").map((def) => def.code),
  );
  for (const [code, key] of MODIFIER_KEYS) {
    if (key.side === "right") {
      assert.ok(codes.has(code), `${code} missing from Sym/Nav`);
    }
  }
});

test("the desktop grid puts each side's keys on its side, by code", () => {
  assert.deepEqual(
    DESKTOP_BOTTOM_LEFT.map((def) => def.code),
    ["ControlLeft", "MetaLeft", "AltLeft"],
  );
  assert.deepEqual(
    DESKTOP_BOTTOM_RIGHT.map((def) => def.code),
    ["AltRight", "MetaRight", "ControlRight"],
  );
  assert.equal(DESKTOP_SHIFT_LEFT.code, "ShiftLeft");
  assert.equal(DESKTOP_SHIFT_RIGHT.code, "ShiftRight");
  // Same keycap text on both sides, as on the hardware: the position says which.
  assert.equal(DESKTOP_SHIFT_LEFT.label, DESKTOP_SHIFT_RIGHT.label);
});

test("a modifier inside a combo is not a sticky modifier", () => {
  const altTab = GUI_COMBO_ROW.find((def) => def.label === "Alt+Tab");
  assert.ok(altTab);
  assert.equal(modifierOf(altTab), null);
});

test("either Shift shifts the displayed glyphs", () => {
  assert.equal(shiftHeld(new Set()), false);
  assert.equal(shiftHeld(new Set(["ShiftLeft"])), true);
  assert.equal(shiftHeld(new Set(["ShiftRight", "AltRight"])), true);
  assert.equal(shiftHeld(new Set(["AltRight"])), false);
});

test("no row repeats a code, so every key has a distinct React key", () => {
  for (const row of [...PRIMARY_SCREEN_ROWS, ...SECONDARY_SCREEN_ROWS]) {
    const codes = row
      .filter((def) => def.type !== "combo")
      .map((def) => def.code);
    assert.equal(new Set(codes).size, codes.length);
  }
});
