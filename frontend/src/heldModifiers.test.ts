// Run with `bun test src/heldModifiers.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { HeldModifiers, type ModifierFlags } from "./heldModifiers.ts";

const NONE: ModifierFlags = {
  shift: false,
  control: false,
  alt: false,
  meta: false,
};
const flags = (...down: (keyof ModifierFlags)[]): ModifierFlags => ({
  ...NONE,
  ...Object.fromEntries(down.map((family) => [family, true])),
});

test("a modifier whose keyup never arrived lapses on the next event", () => {
  const held = new HeldModifiers();
  assert.deepEqual(held.key("MetaLeft", true, flags("meta")), []);
  assert.deepEqual(held.lapsed(flags("meta")), []);
  // The system kept the chord and the keyup with it; the pointer comes back
  // over the desktop with nothing held.
  assert.deepEqual(held.lapsed(NONE), ["MetaLeft"]);
  assert.deepEqual(held.lapsed(NONE), []);
});

test("a keyup that does arrive is not released a second time", () => {
  const held = new HeldModifiers();
  held.key("ShiftLeft", true, flags("shift"));
  assert.deepEqual(held.key("ShiftLeft", false, NONE), []);
  assert.deepEqual(held.lapsed(NONE), []);
});

test("a modifier's own keydown cannot lapse it, whatever its flags say", () => {
  const held = new HeldModifiers();
  assert.deepEqual(held.key("ControlLeft", true, NONE), []);
  assert.deepEqual(held.lapsed(flags("control")), []);
});

test("one side's release leaves the other side held", () => {
  const held = new HeldModifiers();
  held.key("ShiftLeft", true, flags("shift"));
  held.key("ShiftRight", true, flags("shift"));
  assert.deepEqual(held.key("ShiftLeft", false, flags("shift")), []);
  assert.deepEqual(held.lapsed(NONE), ["ShiftRight"]);
});

test("both sides lapse together, and only the family that is up", () => {
  const held = new HeldModifiers();
  held.key("AltLeft", true, flags("alt"));
  held.key("AltRight", true, flags("alt"));
  held.key("ControlLeft", true, flags("alt", "control"));
  assert.deepEqual(held.key("KeyA", true, flags("control")), [
    "AltLeft",
    "AltRight",
  ]);
  assert.deepEqual(held.lapsed(flags("control")), []);
});

test("keys that are not modifiers are not followed", () => {
  const held = new HeldModifiers();
  held.key("KeyA", true, NONE);
  assert.deepEqual(held.lapsed(NONE), []);
});

test("clear forgets what the caller released itself", () => {
  const held = new HeldModifiers();
  held.key("MetaRight", true, flags("meta"));
  held.clear();
  assert.deepEqual(held.lapsed(NONE), []);
});
