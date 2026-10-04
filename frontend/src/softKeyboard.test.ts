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
      assert.equal(cell.commit, "tap", cell.id);
    }
  }
});

test("the ABC shortcut row has no modifier and no Ctrl chord, and the Sym row is the F-keys", () => {
  const cells = PAGE_ABC.rows[0].cells;
  assert.ok(cells.every((cell) => modifierOf(cell.def) === null));
  const sym = PAGE_SYM.rows[0].cells
    .slice(1)
    .map((cell) => (cell.def.type === "special" ? cell.def.code : ""));
  assert.deepEqual(
    sym,
    Array.from({ length: 12 }, (_, i) => `F${i + 1}`),
  );
  // Ctrl and one key is the strip's Ctrl and that key; only a three-finger
  // chord earns a key of its own.
  for (const cell of cells) {
    if (cell.def.type === "combo" && cell.def.codes[0] === "ControlLeft") {
      assert.ok(cell.def.codes.length > 2, cell.def.label);
    }
  }
});

test("only the shortcut row commits on a tap", () => {
  for (const page of PAGES.values()) {
    for (const row of [...page.rows, ...page.side]) {
      for (const cell of row.cells) {
        assert.equal(cell.commit === "tap", row.kind === "shortcut", cell.id);
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

test("the strip has Esc between its modifiers and its arrows", () => {
  const strip = PAGE_ABC.rows.find((row) => row.kind === "strip");
  assert.ok(strip);
  assert.deepEqual(
    strip.cells.map((cell) =>
      cell.def.type === "special" ? cell.def.code : "",
    ),
    [
      "Tab",
      "ControlLeft",
      "AltLeft",
      "MetaLeft",
      "Escape",
      "ArrowLeft",
      "ArrowUp",
      "ArrowDown",
      "ArrowRight",
    ],
  );
  // The shortcut row above no longer carries it.
  assert.ok(
    PAGE_ABC.rows[0].cells.every(
      (cell) => cell.def.type !== "special" || cell.def.code !== "Escape",
    ),
  );
});

test("Backspace ends the same row on both phone pages", () => {
  for (const page of PHONE_PAGES) {
    const last = page.rows[5].cells.at(-1);
    assert.ok(last, page.id);
    assert.equal(
      last.def.type === "special" ? last.def.code : "",
      "Backspace",
      page.id,
    );
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

test("every page has one Sticky key: the PC grid's in the Caps Lock slot, a phone page's leading its shortcut row", () => {
  for (const page of PAGES.values()) {
    const ids = [...cellsOf(page).values()]
      .filter((cell) => cell.def.type === "sticky")
      .map((cell) => cell.id);
    assert.deepEqual(
      ids,
      [page.id === "pc" ? "pc:3:0" : `${page.id}:0:0`],
      page.id,
    );
  }
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
