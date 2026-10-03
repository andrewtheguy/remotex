// What the layout data promises the panel and the press engine. Run with
// `bun test src/softKeyboard.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  cellsOf,
  type LayoutPage,
  labelOf,
  MODIFIER_KEYS,
  modifierOf,
  PAGE_ABC,
  PAGE_PC,
  PAGE_SYM,
  PAGES,
  REPEATING_CODES,
  shiftHeld,
} from "./softKeyboard.ts";

const PHONE_PAGES = [PAGE_ABC, PAGE_SYM];

const codesOn = (page: LayoutPage) =>
  [...cellsOf(page).values()]
    .map((cell) => cell.def)
    .filter((def) => def.type === "special")
    .map((def) => def.code);

const sidesOn = (page: LayoutPage) =>
  new Set(
    [...cellsOf(page).values()]
      .map((cell) => modifierOf(cell.def)?.side)
      .filter((side) => side !== undefined),
  );

test("every phone row measures ten units, so the rows line up", () => {
  for (const page of PHONE_PAGES) {
    for (const row of page.rows) {
      if (row.kind === "shortcut") {
        continue;
      }
      const units = row.cells.reduce((sum, cell) => sum + cell.units, 0);
      assert.equal(units, 10, `${page.id} ${row.kind} row`);
    }
  }
});

test("the phone pages have the same rows, so the keyboard keeps its height", () => {
  assert.equal(PAGE_ABC.rows.length, PAGE_SYM.rows.length);
  assert.deepEqual(
    PAGE_ABC.rows.map((row) => row.kind),
    PAGE_SYM.rows.map((row) => row.kind),
  );
  for (const page of PHONE_PAGES) {
    for (const cell of page.rows[0].cells) {
      assert.ok(cell.commit === "tap" || cell.commit === "hold", cell.id);
    }
  }
});

test("each shortcut row leads with a held Shift, Ctrl, Alt and Super, and ABC has no Ctrl chord", () => {
  for (const page of PHONE_PAGES) {
    const row = page.rows[0].cells;
    assert.deepEqual(
      row
        .filter((cell) => cell.commit === "hold")
        .map((cell) => (cell.def.type === "special" ? cell.def.code : "")),
      ["ShiftLeft", "ControlLeft", "AltLeft", "MetaLeft"],
      page.id,
    );
    assert.deepEqual(
      row.slice(0, 4).map((cell) => cell.commit),
      ["hold", "hold", "hold", "hold"],
      page.id,
    );
  }
  // The Sym row's F-keys sit beside them, so Alt+F4 is a held Alt and F4.
  const sym = PAGE_SYM.rows[0].cells.map((cell) =>
    cell.def.type === "special" ? cell.def.code : "",
  );
  assert.deepEqual(sym.slice(4, 6), ["F1", "F2"]);
  assert.equal(sym.length, 16);
  const cells = PAGE_ABC.rows[0].cells;
  // Ctrl and one key is the strip's Ctrl and that key; only a three-finger
  // chord earns a key of its own.
  for (const cell of cells) {
    if (cell.def.type === "combo" && cell.def.codes[0] === "ControlLeft") {
      assert.ok(cell.def.codes.length > 2, cell.def.label);
    }
  }
});

test("a held cell is always a modifier, and only the shortcut row has one", () => {
  for (const page of PAGES.values()) {
    for (const row of [...page.rows, ...page.side]) {
      for (const cell of row.cells) {
        if (cell.commit === "hold") {
          assert.notEqual(modifierOf(cell.def), null, cell.id);
          assert.equal(row.kind, "shortcut", cell.id);
        }
      }
    }
  }
});

test("no page repeats a cell id", () => {
  for (const page of PAGES.values()) {
    const ids = [...page.rows, ...page.side].flatMap((row) =>
      row.cells.map((cell) => cell.id),
    );
    assert.equal(new Set(ids).size, ids.length, page.id);
    assert.equal(cellsOf(page).size, ids.length, page.id);
  }
});

test("the ABC page holds only left-hand modifiers", () => {
  assert.deepEqual([...sidesOn(PAGE_ABC)], ["left"]);
});

test("the Sym/Nav page holds every right-hand modifier", () => {
  const codes = new Set(codesOn(PAGE_SYM));
  for (const [code, key] of MODIFIER_KEYS) {
    if (key.side === "right") {
      assert.ok(codes.has(code), `${code} missing from Sym/Nav`);
    }
  }
});

test("the strip is the same on every phone page", () => {
  const strip = (page: LayoutPage) =>
    page.rows
      .filter((row) => row.kind === "strip")
      .flatMap((row) => row.cells.map((cell) => cell.def));
  assert.deepEqual(strip(PAGE_SYM), strip(PAGE_ABC));
  assert.ok(strip(PAGE_ABC).length > 0);
});

test("the PC grid puts each side's keys on its side, by code", () => {
  const bottom = PAGE_PC.rows[PAGE_PC.rows.length - 1].cells
    .map((cell) => cell.def)
    .filter((def) => def.type === "special")
    .map((def) => def.code);
  assert.deepEqual(bottom, [
    "ControlLeft",
    "MetaLeft",
    "AltLeft",
    "Space",
    "AltRight",
    "MetaRight",
    "ControlRight",
  ]);
  const shifts = PAGE_PC.rows[4].cells.filter(
    (cell) =>
      cell.def.type === "special" && modifierOf(cell.def)?.kind === "shift",
  );
  assert.deepEqual(
    shifts.map((cell) => (cell.def.type === "special" ? cell.def.code : "")),
    ["ShiftLeft", "ShiftRight"],
  );
  // Same keycap text on both sides, as on the hardware: the position says which.
  assert.equal(labelOf(shifts[0].def, false), labelOf(shifts[1].def, false));
});

test("the keys that repeat are exactly the editing and cursor keys, never a character", () => {
  for (const page of PAGES.values()) {
    for (const cell of cellsOf(page).values()) {
      const repeats =
        cell.def.type === "special" && REPEATING_CODES.has(cell.def.code);
      assert.equal(cell.commit === "down", repeats, cell.id);
      if (cell.def.type === "printable") {
        assert.notEqual(cell.commit, "down", cell.id);
      }
    }
  }
  assert.ok(!REPEATING_CODES.has("Enter"));
});

test("a modifier inside a combo is not a modifier key", () => {
  const altTab = [...cellsOf(PAGE_ABC).values()].find(
    (cell) => cell.def.type === "combo" && cell.def.label === "Alt+Tab",
  );
  assert.ok(altTab);
  assert.equal(modifierOf(altTab.def), null);
});

test("either Shift shifts the displayed glyphs, and a shifted key never does", () => {
  assert.equal(shiftHeld([]), false);
  assert.equal(shiftHeld(["ShiftLeft"]), true);
  assert.equal(shiftHeld(["ShiftRight", "AltRight"]), true);
  assert.equal(shiftHeld(["AltRight"]), false);
  assert.equal(
    labelOf(
      { type: "printable", label: "1", code: "Digit1", shiftLabel: "!" },
      true,
    ),
    "!",
  );
  assert.equal(
    labelOf({ type: "printable", label: "a", code: "KeyA" }, true),
    "A",
  );
  assert.equal(
    labelOf(
      { type: "printable", label: "_", code: "Minus", shifted: true },
      true,
    ),
    "_",
  );
});
