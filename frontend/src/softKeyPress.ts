// The soft keyboard's press engine: fingers in, key presses out.
//
// One engine serves the whole key area instead of a handler per key. It is pure
// — abstract pointer samples and a hit tester in, commands out, time passed in
// with every event — so that what a tap, a slide, a hold and two thumbs at once
// do is decided here, tested here, and the panel only wires the DOM to it.
//
// Four ways a cell commits (softKeyboard.ts `Commit`):
// - `lift`: the key under the finger when it lifts. The finger may slide to a
//   neighbour first; the key it is over is shown as it goes. Lifting off the
//   keyboard commits nothing, and so does a cancel — with `touch-action: none`
//   on the key area, a cancel means the system took the finger (an edge swipe),
//   not that the browser is scrolling.
// - `down`: at once on touch, then repeating while held — Backspace and the
//   cursor keys. Every repeat is wrapped with the modifiers held *at that tick*.
// - `tap`: the scrollable shortcut row, where a slide is the row scrolling. The
//   key commits on lift if the finger stayed within the slop. A cancel inside the
//   slop commits too: the browser cancels a touch the moment it claims a pan, and
//   its own slop can trip before ours, which used to swallow the tap.
// - `hold`: a modifier that is a key on the wire — down when touched, up when
//   the finger lifts or is taken. The remote holds it under whatever happens
//   meanwhile: the other thumb's keys, a tap on the canvas, a physical key.
//
// The other modifier keys never reach the wire on their own. A tap arms a
// one-shot, which the next commit spends, a repeat tick included, and a second
// tap disarms it. Under a resting finger a modifier chords the other fingers'
// keys like a physical chord, and is off when the finger lifts. Every such
// modifier is sent down ahead of the key in the order it was taken, and
// released after it, through sendKeyCombo; one the wire already holds is left
// out, since the remote has it.
import {
  type CellId,
  type Commit,
  type LayoutCell,
  modifierOf,
  type PageId,
  type SoftKeyDefinition,
} from "./softKeyboard.ts";

export type PointerKind = "touch" | "pen" | "mouse";

export interface PointerSample {
  id: number;
  kind: PointerKind;
  x: number;
  y: number;
  t: number;
}

export type PressEvent =
  // A finger landed. `cell` is what the DOM says is under it, or null when it
  // landed between keys; the engine then asks the hit tester.
  | { kind: "down"; p: PointerSample; cell: CellId | null }
  | { kind: "move"; p: PointerSample }
  | { kind: "up"; p: PointerSample }
  // pointercancel or lostpointercapture for one pointer.
  | { kind: "cancel"; id: number; t: number }
  // The page lost focus or was hidden: every finger is forgotten, nothing sends,
  // and whatever the wire holds is released.
  | { kind: "cancelAll"; t: number }
  // The host's timer, due at the last result's `nextTickAt`.
  | { kind: "tick"; t: number }
  // The cells changed under the fingers (a page switch).
  | { kind: "layout"; cells: ReadonlyMap<CellId, LayoutCell> };

// `oneShot` is armed for the next key; `held` is a finger resting on the key,
// chording; `down` is a `hold` key pressed on the wire under a finger.
export type ModifierState = "oneShot" | "held" | "down";

export type PressCommand =
  // Press these codes in order and release them in reverse: the chording
  // modifiers, then the key's own codes.
  | { kind: "send"; codes: string[] }
  // Press or release one code and leave it so: a `hold` modifier.
  | { kind: "key"; code: string; pressed: boolean }
  | { kind: "modifiers"; held: ReadonlyMap<string, ModifierState> }
  // The cells currently under a finger.
  | { kind: "active"; ids: ReadonlySet<CellId> }
  // The key a touch is resting on and would commit if it lifted now, or null
  // when it has lifted or slid off. Mouse pointers never get one.
  | { kind: "preview"; pointerId: number; id: CellId | null }
  | { kind: "page"; page: PageId }
  // A commit happened under a touch: the host may vibrate.
  | { kind: "haptic" };

// Which cell is under a point, preferring `prefer` while the point stays within
// its slop. Null when the point is off the keyboard.
export type HitTester = (
  x: number,
  y: number,
  prefer: CellId | null,
) => CellId | null;

export interface StepResult {
  commands: PressCommand[];
  // When the host should next send a `tick`, or null when nothing is pending.
  nextTickAt: number | null;
}

export interface PressOptions {
  repeatDelayMs: number;
  repeatIntervalMs: number;
  tapSlopPx: number;
}

// 400/80 is the typematic feel of a physical keyboard. Each repeat is a whole
// press and release, so the remote's own repeat never engages on top of it.
export const DEFAULT_PRESS_OPTIONS: PressOptions = {
  repeatDelayMs: 400,
  repeatIntervalMs: 80,
  tapSlopPx: 8,
};

export interface PressEngine {
  handle(event: PressEvent): StepResult;
  modifiers(): ReadonlyMap<string, ModifierState>;
}

interface Press {
  id: number;
  kind: PointerKind;
  commit: Commit;
  // The cell this finger would commit, or null when it has slid off.
  cell: CellId | null;
  startX: number;
  startY: number;
  // The modifier this finger is resting on, and what it was before.
  modifier: string | null;
  previous: ModifierState | undefined;
  // Another finger committed while this modifier was held.
  chorded: boolean;
  // When a `down` cell next repeats.
  nextRepeatAt: number | null;
  // The last preview shown for this finger.
  previewed: CellId | null;
}

function codesOf(def: SoftKeyDefinition): string[] | null {
  switch (def.type) {
    case "printable":
      return def.shifted ? ["ShiftLeft", def.code] : [def.code];
    case "special":
      return [def.code];
    case "combo":
      return def.codes;
    default:
      return null;
  }
}

function sameSet(a: ReadonlySet<string>, b: ReadonlySet<string>): boolean {
  if (a.size !== b.size) {
    return false;
  }
  for (const v of a) {
    if (!b.has(v)) {
      return false;
    }
  }
  return true;
}

function sameModifiers(
  a: ReadonlyMap<string, ModifierState>,
  b: ReadonlyMap<string, ModifierState>,
): boolean {
  if (a.size !== b.size) {
    return false;
  }
  const ea = [...a];
  const eb = [...b];
  return ea.every(([k, v], i) => eb[i][0] === k && eb[i][1] === v);
}

export function createPressEngine(
  hit: HitTester,
  cells: ReadonlyMap<CellId, LayoutCell>,
  options: Partial<PressOptions> = {},
): PressEngine {
  const opts = { ...DEFAULT_PRESS_OPTIONS, ...options };
  let layout = cells;
  const presses = new Map<number, Press>();
  // Insertion order is the order the modifiers go down on the wire.
  const held = new Map<string, ModifierState>();
  let shownActive: ReadonlySet<CellId> = new Set();
  let shownModifiers: ReadonlyMap<string, ModifierState> = new Map();

  const cellOf = (id: CellId | null): LayoutCell | null =>
    id === null ? null : (layout.get(id) ?? null);

  const modifierCodeOf = (id: CellId | null): string | null => {
    const cell = cellOf(id);
    if (!cell || cell.def.type !== "special") {
      return null;
    }
    return modifierOf(cell.def) ? cell.def.code : null;
  };

  // Commands are gathered per event; `active` and `modifiers` are emitted at the
  // end only if they changed, so the host never re-renders for nothing.
  let out: PressCommand[] = [];

  const preview = (press: Press, id: CellId | null) => {
    if (press.kind === "mouse" || press.commit !== "lift") {
      return;
    }
    if (press.previewed === id) {
      return;
    }
    press.previewed = id;
    out.push({ kind: "preview", pointerId: press.id, id });
  };

  const haptic = (press: Press) => {
    if (press.kind !== "mouse") {
      out.push({ kind: "haptic" });
    }
  };

  // Arm or disarm a modifier the way a tap does.
  const toggle = (code: string, from: ModifierState | undefined) => {
    if (from === undefined) {
      held.set(code, "oneShot");
    } else {
      held.delete(code);
    }
  };

  // A finger lifted off a modifier it was resting on.
  const releaseModifier = (press: Press, cancelled: boolean) => {
    const code = press.modifier;
    if (code === null) {
      return;
    }
    press.modifier = null;
    if (press.commit === "hold") {
      held.delete(code);
      out.push({ kind: "key", code, pressed: false });
      return;
    }
    if (cancelled) {
      if (press.previous === undefined) {
        held.delete(code);
      } else {
        held.set(code, press.previous);
      }
      return;
    }
    if (press.chorded) {
      held.delete(code);
      return;
    }
    toggle(code, press.previous);
  };

  // The finger resting on this modifier has been used as a chord.
  const markChorded = (code: string) => {
    for (const other of presses.values()) {
      if (other.modifier === code) {
        other.chorded = true;
      }
    }
  };

  // A key has been sent: the one-shots are spent, the resting fingers chorded.
  const spend = () => {
    for (const [code, state] of [...held]) {
      if (state === "oneShot") {
        held.delete(code);
      } else if (state === "held") {
        markChorded(code);
      }
    }
  };

  // Send a key with the chording modifiers around it. A modifier the wire
  // holds is dropped from both: pressing it again would release it under the
  // finger that holds it, and the remote has it anyway.
  const send = (press: Press, codes: string[]) => {
    const own = codes.filter((code) => held.get(code) !== "down");
    if (own.length === 0) {
      return;
    }
    const wrap = [...held]
      .filter(([code, state]) => state !== "down" && !own.includes(code))
      .map(([code]) => code);
    out.push({ kind: "send", codes: [...wrap, ...own] });
    spend();
    haptic(press);
  };

  // A page key: the keys under every other finger are about to change.
  const switchPage = (press: Press, page: PageId) => {
    out.push({ kind: "page", page });
    haptic(press);
    for (const other of presses.values()) {
      if (other !== press && other.commit === "lift") {
        other.cell = null;
        preview(other, null);
      }
    }
  };

  // What a committed cell does.
  const commit = (press: Press, id: CellId) => {
    const cell = cellOf(id);
    if (!cell) {
      return;
    }
    const code = modifierCodeOf(id);
    if (code !== null) {
      // Only a `tap` modifier commits this way; the others act on touch.
      toggle(code, held.get(code));
      haptic(press);
      return;
    }
    if (cell.def.type === "page") {
      switchPage(press, cell.def.page);
      return;
    }
    const codes = codesOf(cell.def);
    if (codes) {
      send(press, codes);
    }
  };

  const forget = (press: Press) => {
    preview(press, null);
    presses.delete(press.id);
  };

  // A finger landed on a modifier that acts on touch: a `hold` key goes down on
  // the wire, any other is held for a chord. One already under another finger
  // is that finger's; this one does nothing.
  const takeModifier = (press: Press, code: string): boolean => {
    const state = held.get(code);
    if (state === "held" || state === "down") {
      return false;
    }
    press.modifier = code;
    press.previous = state;
    held.delete(code);
    if (press.commit === "hold") {
      held.set(code, "down");
      out.push({ kind: "key", code, pressed: true });
    } else {
      held.set(code, "held");
    }
    haptic(press);
    return true;
  };

  const down = (p: PointerSample, target: CellId | null) => {
    const stale = presses.get(p.id);
    if (stale) {
      // A pointer the host never saw lift: it is over, silently.
      releaseModifier(stale, true);
      forget(stale);
    }
    const id = target ?? hit(p.x, p.y, null);
    const cell = cellOf(id);
    if (!cell || id === null) {
      return;
    }
    const press: Press = {
      id: p.id,
      kind: p.kind,
      commit: cell.commit,
      cell: id,
      startX: p.x,
      startY: p.y,
      modifier: null,
      previous: undefined,
      chorded: false,
      nextRepeatAt: null,
      previewed: null,
    };
    const code = modifierCodeOf(id);
    if (code !== null && cell.commit !== "tap") {
      if (takeModifier(press, code)) {
        presses.set(p.id, press);
      }
      return;
    }
    presses.set(p.id, press);
    switch (cell.commit) {
      case "down":
        commit(press, id);
        press.nextRepeatAt = p.t + opts.repeatDelayMs;
        break;
      case "lift":
        preview(press, id);
        break;
      case "tap":
      case "hold":
        break;
    }
  };

  // A finger sliding on a lift key: the key under it follows. A slide only
  // moves between keys that commit on lift; onto a modifier or a repeating key
  // it is off the key instead.
  const slide = (press: Press, p: PointerSample) => {
    const next = hit(p.x, p.y, press.cell);
    const cell = cellOf(next);
    const id =
      cell && cell.commit === "lift" && !modifierCodeOf(next) ? next : null;
    if (id !== press.cell) {
      press.cell = id;
      preview(press, id);
    }
  };

  // A finger on the shortcut row that has travelled is the row scrolling.
  const trackTap = (press: Press, p: PointerSample) => {
    if (
      press.cell !== null &&
      (Math.abs(p.x - press.startX) > opts.tapSlopPx ||
        Math.abs(p.y - press.startY) > opts.tapSlopPx)
    ) {
      press.cell = null;
    }
  };

  const move = (p: PointerSample) => {
    const press = presses.get(p.id);
    if (!press || press.modifier !== null) {
      return;
    }
    if (press.commit === "lift") {
      slide(press, p);
    } else if (press.commit === "tap") {
      trackTap(press, p);
    }
  };

  const up = (p: PointerSample) => {
    const press = presses.get(p.id);
    if (!press) {
      return;
    }
    if (press.modifier !== null) {
      releaseModifier(press, false);
      haptic(press);
    } else if (press.commit !== "down" && press.cell !== null) {
      commit(press, press.cell);
    }
    forget(press);
  };

  const cancel = (id: number) => {
    const press = presses.get(id);
    if (!press) {
      return;
    }
    if (press.modifier !== null) {
      releaseModifier(press, true);
    } else if (press.commit === "tap" && press.cell !== null) {
      commit(press, press.cell);
    }
    forget(press);
  };

  const tick = (t: number) => {
    for (const press of presses.values()) {
      if (press.nextRepeatAt !== null && t >= press.nextRepeatAt) {
        if (press.cell !== null) {
          commit(press, press.cell);
        }
        press.nextRepeatAt = t + opts.repeatIntervalMs;
      }
    }
  };

  const relayout = (next: ReadonlyMap<CellId, LayoutCell>) => {
    layout = next;
    for (const press of presses.values()) {
      if (press.cell !== null && !layout.has(press.cell)) {
        press.cell = null;
        press.nextRepeatAt = null;
        preview(press, null);
      }
    }
  };

  // The earliest moment any finger needs a tick, or null.
  const nextTick = (): number | null => {
    let at: number | null = null;
    for (const press of presses.values()) {
      const due = press.nextRepeatAt;
      if (due !== null && (at === null || due < at)) {
        at = due;
      }
    }
    return at;
  };

  const finish = (): StepResult => {
    const active = new Set<CellId>();
    for (const press of presses.values()) {
      if (press.cell !== null) {
        active.add(press.cell);
      }
    }
    if (!sameSet(active, shownActive)) {
      shownActive = active;
      out.push({ kind: "active", ids: active });
    }
    if (!sameModifiers(held, shownModifiers)) {
      shownModifiers = new Map(held);
      out.push({ kind: "modifiers", held: shownModifiers });
    }
    const commands = out;
    out = [];
    return { commands, nextTickAt: nextTick() };
  };

  return {
    handle(event) {
      switch (event.kind) {
        case "down":
          down(event.p, event.cell);
          break;
        case "move":
          move(event.p);
          break;
        case "up":
          up(event.p);
          break;
        case "cancel":
          cancel(event.id);
          break;
        case "cancelAll":
          for (const press of [...presses.values()]) {
            releaseModifier(press, true);
            forget(press);
          }
          break;
        case "tick":
          tick(event.t);
          break;
        case "layout":
          relayout(event.cells);
          break;
      }
      return finish();
    },
    modifiers() {
      return new Map(held);
    },
  };
}
