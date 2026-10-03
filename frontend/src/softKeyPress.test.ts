// What a finger does on the soft keyboard, decided without a DOM: the hit
// tester is a table, time is the number each event carries. Run with
// `bun test src/softKeyPress.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import type {
  CellId,
  Commit,
  LayoutCell,
  SoftKeyDefinition,
} from "./softKeyboard.ts";
import {
  createPressEngine,
  type HitTester,
  type PointerKind,
  type PressCommand,
  type PressEngine,
} from "./softKeyPress.ts";

// A one-row keyboard of 40px cells at y 0–40; cell `i` spans x [40i, 40i+40).
// A point above or below the row is off the keyboard.
function cell(id: CellId, def: SoftKeyDefinition, commit: Commit): LayoutCell {
  return { id, def, units: 1, commit };
}

const CELLS: LayoutCell[] = [
  cell("a", { type: "printable", label: "a", code: "KeyA" }, "lift"),
  cell("s", { type: "printable", label: "s", code: "KeyS" }, "lift"),
  cell("shift", { type: "special", label: "Shift", code: "ShiftLeft" }, "lift"),
  cell("bksp", { type: "special", label: "Bksp", code: "Backspace" }, "down"),
  cell("sym", { type: "page", label: "?123", page: "sym" }, "lift"),
  cell(
    "altTab",
    { type: "combo", label: "Alt+Tab", codes: ["AltLeft", "Tab"] },
    "tap",
  ),
  cell("super", { type: "special", label: "Super", code: "MetaLeft" }, "tap"),
  cell(
    "under",
    { type: "printable", label: "_", code: "Minus", shifted: true },
    "lift",
  ),
  cell("ctrl", { type: "special", label: "Ctrl", code: "ControlLeft" }, "lift"),
  // The shortcut row's held modifiers: keys on the wire while touched.
  cell(
    "hctrl",
    { type: "special", label: "Ctrl", code: "ControlLeft" },
    "hold",
  ),
  cell(
    "hshift",
    { type: "special", label: "Shift", code: "ShiftLeft" },
    "hold",
  ),
];

const LAYOUT = new Map(CELLS.map((c) => [c.id, c]));
const xOf = (id: CellId) => CELLS.findIndex((c) => c.id === id) * 40 + 20;

const hit: HitTester = (x, y) => {
  if (y < 0 || y > 40 || x < 0) {
    return null;
  }
  const i = Math.floor(x / 40);
  return i < CELLS.length ? CELLS[i].id : null;
};

// A driver that speaks in cells: `down("a")` lands on cell a's centre.
class Fingers {
  readonly engine: PressEngine;
  readonly sent: string[][] = [];
  // Every press and release of a held modifier, as [code, pressed].
  readonly keys: [string, boolean][] = [];
  readonly log: PressCommand[] = [];
  nextTickAt: number | null = null;
  haptics = 0;

  constructor(engine = createPressEngine(hit, LAYOUT)) {
    this.engine = engine;
  }

  private run(event: Parameters<PressEngine["handle"]>[0]) {
    const { commands, nextTickAt } = this.engine.handle(event);
    this.nextTickAt = nextTickAt;
    for (const c of commands) {
      this.log.push(c);
      if (c.kind === "send") {
        this.sent.push(c.codes);
      }
      if (c.kind === "key") {
        this.keys.push([c.code, c.pressed]);
      }
      if (c.kind === "haptic") {
        this.haptics += 1;
      }
    }
    return commands;
  }

  down(id: CellId, t = 0, pointer = 1, kind: PointerKind = "touch") {
    return this.run({
      kind: "down",
      p: { id: pointer, kind, x: xOf(id), y: 20, t },
      cell: id,
    });
  }
  // A finger landing between keys: the DOM names no cell.
  downAt(x: number, y: number, t = 0, pointer = 1) {
    return this.run({
      kind: "down",
      p: { id: pointer, kind: "touch", x, y, t },
      cell: null,
    });
  }
  moveTo(x: number, y: number, t = 0, pointer = 1) {
    return this.run({
      kind: "move",
      p: { id: pointer, kind: "touch", x, y, t },
    });
  }
  upAt(x: number, y: number, t = 0, pointer = 1) {
    return this.run({ kind: "up", p: { id: pointer, kind: "touch", x, y, t } });
  }
  up(id: CellId, t = 0, pointer = 1) {
    return this.upAt(xOf(id), 20, t, pointer);
  }
  tap(id: CellId, t = 0, pointer = 1) {
    this.down(id, t, pointer);
    return this.up(id, t + 50, pointer);
  }
  cancel(pointer = 1, t = 0) {
    return this.run({ kind: "cancel", id: pointer, t });
  }
  cancelAll(t = 0) {
    return this.run({ kind: "cancelAll", t });
  }
  tick(t: number) {
    return this.run({ kind: "tick", t });
  }
  layout(cells: ReadonlyMap<CellId, LayoutCell>) {
    return this.run({ kind: "layout", cells });
  }
  previews(): (CellId | null)[] {
    return this.log
      .filter((c) => c.kind === "preview")
      .map((c) => (c.kind === "preview" ? c.id : null));
  }
  actives(): string[][] {
    return this.log
      .filter((c) => c.kind === "active")
      .map((c) => (c.kind === "active" ? [...c.ids] : []));
  }
  modifiers(): [string, string][] {
    return [...this.engine.modifiers()];
  }
}

test("a tap commits once, on lift, and nothing on touch", () => {
  const f = new Fingers();
  f.down("a");
  assert.deepEqual(f.sent, []);
  f.up("a", 60);
  assert.deepEqual(f.sent, [["KeyA"]]);
  assert.equal(f.nextTickAt, null);
});

test("a finger that slides to a neighbour commits the neighbour and previews both", () => {
  const f = new Fingers();
  f.down("a");
  f.moveTo(xOf("s"), 20, 30);
  f.upAt(xOf("s"), 20, 60);
  assert.deepEqual(f.sent, [["KeyS"]]);
  assert.deepEqual(f.previews(), ["a", "s", null]);
});

test("a finger that slides off the keyboard commits nothing", () => {
  const f = new Fingers();
  f.down("a");
  f.moveTo(xOf("a"), 120, 30);
  f.upAt(xOf("a"), 120, 60);
  assert.deepEqual(f.sent, []);
  assert.deepEqual(f.previews(), ["a", null]);
  assert.deepEqual(f.actives().at(-1), []);
});

test("a cancel mid-press commits nothing", () => {
  const f = new Fingers();
  f.down("a");
  f.cancel();
  assert.deepEqual(f.sent, []);
  assert.deepEqual(f.previews(), ["a", null]);
});

test("two thumbs overlapping commit both, in the order they lift", () => {
  const f = new Fingers();
  f.down("a", 0, 1);
  f.down("s", 10, 2);
  f.up("s", 40, 2);
  f.up("a", 50, 1);
  assert.deepEqual(f.sent, [["KeyS"], ["KeyA"]]);
});

test("a finger landing between keys is placed by the hit tester", () => {
  const f = new Fingers();
  f.downAt(xOf("s"), 20);
  f.upAt(xOf("s"), 20, 50);
  assert.deepEqual(f.sent, [["KeyS"]]);
});

test("a repeating key sends on touch, then after the delay at each interval", () => {
  const f = new Fingers();
  f.down("bksp", 1000);
  assert.deepEqual(f.sent, [["Backspace"]]);
  assert.equal(f.nextTickAt, 1400);
  f.tick(1399);
  assert.equal(f.sent.length, 1);
  f.tick(1400);
  assert.equal(f.sent.length, 2);
  assert.equal(f.nextTickAt, 1480);
  f.tick(1480);
  assert.equal(f.sent.length, 3);
  f.up("bksp", 1500);
  assert.equal(f.sent.length, 3);
  assert.equal(f.nextTickAt, null);
});

test("a cancel stops the repeat", () => {
  const f = new Fingers();
  f.down("bksp", 0);
  f.tick(400);
  f.cancel(1, 450);
  assert.equal(f.nextTickAt, null);
  assert.equal(f.sent.length, 2);
});

test("a one-shot modifier wraps the next key and is then spent", () => {
  const f = new Fingers();
  f.tap("shift");
  assert.deepEqual(f.modifiers(), [["ShiftLeft", "oneShot"]]);
  f.tap("a", 100);
  assert.deepEqual(f.sent, [["ShiftLeft", "KeyA"]]);
  assert.deepEqual(f.modifiers(), []);
  f.tap("a", 200);
  assert.deepEqual(f.sent.at(-1), ["KeyA"]);
});

test("a one-shot is spent by the first repeat; a resting finger chords every one", () => {
  const f = new Fingers();
  f.tap("shift");
  f.down("bksp", 100);
  f.tick(500);
  f.tick(580);
  assert.deepEqual(f.sent, [
    ["ShiftLeft", "Backspace"],
    ["Backspace"],
    ["Backspace"],
  ]);
  f.up("bksp", 600);

  f.down("ctrl", 700, 1);
  f.down("bksp", 800, 2);
  f.tick(1200);
  assert.deepEqual(f.sent.slice(3), [
    ["ControlLeft", "Backspace"],
    ["ControlLeft", "Backspace"],
  ]);
  f.up("bksp", 1250, 2);
  f.up("ctrl", 1300, 1);
  assert.deepEqual(f.modifiers(), []);
});

test("tapping a modifier arms it and tapping again disarms it; nothing locks", () => {
  const f = new Fingers();
  f.tap("shift", 0);
  assert.deepEqual(f.modifiers(), [["ShiftLeft", "oneShot"]]);
  f.tap("shift", 100);
  assert.deepEqual(f.modifiers(), []);
  f.tap("a", 200);
  assert.deepEqual(f.sent, [["KeyA"]]);

  // A long press is a tap: no timer runs on a resting modifier.
  f.down("shift", 300);
  assert.deepEqual(f.modifiers(), [["ShiftLeft", "held"]]);
  assert.equal(f.nextTickAt, null);
  f.up("shift", 5000);
  assert.deepEqual(f.modifiers(), [["ShiftLeft", "oneShot"]]);
  f.down("shift", 5100);
  f.up("shift", 9000);
  assert.deepEqual(f.modifiers(), []);
});

test("a modifier under a resting finger chords the other finger, then lets go", () => {
  const f = new Fingers();
  f.down("shift", 0, 1);
  f.tap("a", 100, 2);
  assert.deepEqual(f.sent, [["ShiftLeft", "KeyA"]]);
  f.up("shift", 200, 1);
  assert.deepEqual(f.modifiers(), []);
});

test("a chorded modifier is off when the finger lifts, armed or not before", () => {
  const f = new Fingers();
  f.tap("ctrl", 0);
  f.down("ctrl", 200, 1);
  f.tap("a", 300, 2);
  f.up("ctrl", 400, 1);
  assert.deepEqual(f.sent, [["ControlLeft", "KeyA"]]);
  assert.deepEqual(f.modifiers(), []);
});

test("a held modifier is down on the wire while touched, and up when the finger lifts or is taken", () => {
  const f = new Fingers();
  f.down("hctrl", 0);
  assert.deepEqual(f.keys, [["ControlLeft", true]]);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "down"]]);
  assert.equal(f.nextTickAt, null);
  f.up("hctrl", 2000);
  assert.deepEqual(f.keys.at(-1), ["ControlLeft", false]);
  assert.deepEqual(f.modifiers(), []);
  assert.deepEqual(f.sent, []);

  f.down("hctrl", 3000);
  f.cancel(1, 3100);
  assert.deepEqual(f.keys.slice(2), [
    ["ControlLeft", true],
    ["ControlLeft", false],
  ]);
  assert.deepEqual(f.modifiers(), []);
});

test("a key under a held modifier is sent bare, and a one-shot wraps only what the wire lacks", () => {
  const f = new Fingers();
  f.down("hctrl", 0, 1);
  f.tap("a", 100, 2);
  assert.deepEqual(f.sent, [["KeyA"]]);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "down"]]);
  f.tap("shift", 200, 2);
  f.tap("a", 300, 2);
  assert.deepEqual(f.sent.at(-1), ["ShiftLeft", "KeyA"]);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "down"]]);
  f.up("hctrl", 400, 1);

  // A shifted symbol under a held Shift is its code alone.
  f.down("hshift", 500, 1);
  f.tap("under", 600, 2);
  assert.deepEqual(f.sent.at(-1), ["Minus"]);
  f.up("hshift", 700, 1);
  assert.deepEqual(f.keys, [
    ["ControlLeft", true],
    ["ControlLeft", false],
    ["ShiftLeft", true],
    ["ShiftLeft", false],
  ]);
});

test("a held modifier takes over an armed one-shot of the same code", () => {
  const f = new Fingers();
  f.tap("ctrl", 0);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "oneShot"]]);
  f.down("hctrl", 100);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "down"]]);
  f.up("hctrl", 200);
  assert.deepEqual(f.modifiers(), []);
  assert.deepEqual(f.keys, [
    ["ControlLeft", true],
    ["ControlLeft", false],
  ]);
});

test("a modifier under another finger is that finger's; a second touch on it is nothing", () => {
  const f = new Fingers();
  f.down("hctrl", 0, 1);
  f.down("ctrl", 100, 2);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "down"]]);
  f.up("ctrl", 200, 2);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "down"]]);
  f.up("hctrl", 300, 1);
  assert.deepEqual(f.keys, [
    ["ControlLeft", true],
    ["ControlLeft", false],
  ]);

  f.down("ctrl", 400, 1);
  f.down("hctrl", 500, 2);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "held"]]);
  assert.equal(f.keys.length, 2);
  f.up("hctrl", 600, 2);
  f.up("ctrl", 700, 1);
  assert.deepEqual(f.modifiers(), [["ControlLeft", "oneShot"]]);
});

test("a cancel of everything releases what the wire holds", () => {
  const f = new Fingers();
  f.down("hshift", 0, 1);
  f.down("a", 50, 2);
  f.cancelAll(100);
  assert.deepEqual(f.keys, [
    ["ShiftLeft", true],
    ["ShiftLeft", false],
  ]);
  assert.deepEqual(f.sent, []);
  assert.deepEqual(f.modifiers(), []);
});

test("modifiers go down in the order they were taken", () => {
  const f = new Fingers();
  f.tap("ctrl", 0);
  f.tap("shift", 100);
  f.tap("a", 200);
  assert.deepEqual(f.sent, [["ControlLeft", "ShiftLeft", "KeyA"]]);
});

test("a combo is wrapped by held modifiers, without doubling its own", () => {
  const f = new Fingers();
  f.tap("shift", 0);
  f.tap("ctrl", 50);
  f.tap("altTab", 100);
  assert.deepEqual(f.sent, [["ShiftLeft", "ControlLeft", "AltLeft", "Tab"]]);
  f.down("ctrl", 200);
  f.up("ctrl", 250);
  f.tap("altTab", 300);
  // ControlLeft held, and the combo's own AltLeft not repeated.
  assert.deepEqual(f.sent.at(-1), ["ControlLeft", "AltLeft", "Tab"]);
});

test("a shifted symbol sends Shift and the code, once each", () => {
  const f = new Fingers();
  f.tap("under");
  assert.deepEqual(f.sent, [["ShiftLeft", "Minus"]]);
  f.tap("shift", 100);
  f.tap("under", 200);
  assert.deepEqual(f.sent.at(-1), ["ShiftLeft", "Minus"]);
  assert.deepEqual(f.modifiers(), []);
});

test("a shortcut-row key commits on lift within the slop, and on a cancel within it", () => {
  const f = new Fingers();
  f.down("altTab", 0);
  f.moveTo(xOf("altTab") + 5, 22, 20);
  f.upAt(xOf("altTab") + 5, 22, 40);
  assert.deepEqual(f.sent, [["AltLeft", "Tab"]]);

  f.down("altTab", 100);
  f.cancel(1, 120);
  assert.deepEqual(f.sent.length, 2);
});

test("a shortcut-row key that scrolled commits nothing, whoever ended it", () => {
  const f = new Fingers();
  f.down("altTab", 0);
  f.moveTo(xOf("altTab") + 20, 20, 20);
  f.upAt(xOf("altTab") + 20, 20, 40);
  assert.deepEqual(f.sent, []);

  f.down("altTab", 100);
  f.moveTo(xOf("altTab") + 20, 20, 120);
  f.cancel(1, 140);
  assert.deepEqual(f.sent, []);
});

test("a shortcut-row modifier toggles on lift, not on touch", () => {
  const f = new Fingers();
  f.down("super");
  assert.deepEqual(f.modifiers(), []);
  f.up("super", 50);
  assert.deepEqual(f.modifiers(), [["MetaLeft", "oneShot"]]);
});

test("a page key switches on lift and drops the other fingers' keys", () => {
  const f = new Fingers();
  f.down("a", 0, 2);
  f.tap("sym", 10, 1);
  assert.ok(f.log.some((c) => c.kind === "page" && c.page === "sym"));
  f.up("a", 100, 2);
  assert.deepEqual(f.sent, []);
});

test("a layout change under a repeating finger stops the repeat", () => {
  const f = new Fingers();
  f.down("bksp", 0);
  f.layout(new Map());
  assert.equal(f.nextTickAt, null);
  f.tick(400);
  assert.deepEqual(f.sent, [["Backspace"]]);
});

test("a cancel of everything forgets every finger and sends nothing", () => {
  const f = new Fingers();
  f.down("shift", 0, 1);
  f.down("a", 0, 2);
  // Chorded under the resting Shift, as a key pressed then would be.
  f.down("bksp", 0, 3);
  f.cancelAll(10);
  assert.deepEqual(f.sent, [["ShiftLeft", "Backspace"]]);
  assert.deepEqual(f.modifiers(), []);
  assert.equal(f.nextTickAt, null);
  assert.deepEqual(f.actives().at(-1), []);
  f.up("a", 20, 2);
  assert.deepEqual(f.sent.length, 1);
});

test("a cancel after the lift is nothing", () => {
  const f = new Fingers();
  f.tap("a");
  f.cancel(1, 60);
  assert.deepEqual(f.sent, [["KeyA"]]);
});

test("a second touch for a finger that never lifted replaces it without sending", () => {
  const f = new Fingers();
  f.down("a", 0);
  f.down("s", 10);
  f.up("s", 20);
  assert.deepEqual(f.sent, [["KeyS"]]);
});

test("active and preview are reported only when they change", () => {
  const f = new Fingers();
  f.down("a", 0);
  f.moveTo(xOf("a") + 2, 21, 10);
  f.moveTo(xOf("a") - 2, 19, 20);
  assert.deepEqual(f.actives(), [["a"]]);
  assert.deepEqual(f.previews(), ["a"]);
  f.up("a", 30);
  assert.deepEqual(f.actives(), [["a"], []]);
});

test("a mouse gets no preview and no haptic; a touch gets both", () => {
  const f = new Fingers();
  f.down("a", 0, 1, "mouse");
  f.up("a", 10, 1);
  assert.deepEqual(f.previews(), []);
  assert.equal(f.haptics, 0);
  f.tap("a", 100);
  assert.deepEqual(f.previews(), ["a", null]);
  assert.equal(f.haptics, 1);
});

test("a slide onto a modifier or a repeating key is off the key", () => {
  const f = new Fingers();
  f.down("s");
  f.moveTo(xOf("shift"), 20, 10);
  assert.deepEqual(f.previews(), ["s", null]);
  f.moveTo(xOf("bksp"), 20, 20);
  f.upAt(xOf("bksp"), 20, 30);
  assert.deepEqual(f.sent, []);
  assert.deepEqual(f.modifiers(), []);
});
