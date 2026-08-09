// The Command translator's state machine, which is pure logic and so is checked
// here rather than through a browser. What a browser adds — which chords it hands
// to the page at all — no unit test can answer; that is
// tests/playwright/mac-keyboard.spec.ts and manual QA.
//
// Deliberately parallel to the viewer's KeyboardTranslatorTests.swift: the two
// clients translate the same chords into the same codes, and a case that exists
// there and not here is a place they could drift apart. Run with
// `bun test src/macKeys.test.ts` from frontend/.
import assert from "node:assert/strict";
import { test } from "node:test";
import { MacKeyboardTranslator, type TranslatedKey } from "./macKeys.ts";

const down = (code: string, meta = false) => ({
  code,
  pressed: true,
  caps: false,
  meta,
});
const up = (code: string, meta = false) => ({
  code,
  pressed: false,
  caps: false,
  meta,
});
const sent = (code: string, pressed: boolean): TranslatedKey => ({
  code,
  pressed,
  caps: false,
});

test("a mapped Command chord becomes a Control chord with no Command in it", () => {
  const t = new MacKeyboardTranslator();
  // Command's own press is withheld: at this point the chord could still turn
  // out to be one that forwards it.
  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(down("KeyC", true), true), [
    sent("ControlLeft", true),
    sent("KeyC", true),
  ]);
  assert.deepEqual(t.translate(up("KeyC", true), true), [
    sent("KeyC", false),
    sent("ControlLeft", false),
  ]);
  // And Command's release is swallowed too — the guest was never told it was
  // down, so a release would be a Control-less Meta up out of nowhere.
  assert.deepEqual(t.translate(up("MetaLeft"), true), []);
});

test("two mapped chords under one Command share a single Control", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  assert.deepEqual(t.translate(down("KeyC", true), true), [
    sent("ControlLeft", true),
    sent("KeyC", true),
  ]);
  assert.deepEqual(t.translate(up("KeyC", true), true), [
    sent("KeyC", false),
    sent("ControlLeft", false),
  ]);
  assert.deepEqual(t.translate(down("KeyV", true), true), [
    sent("ControlLeft", true),
    sent("KeyV", true),
  ]);
  assert.deepEqual(t.translate(up("KeyV", true), true), [
    sent("KeyV", false),
    sent("ControlLeft", false),
  ]);
});

test("Control is held across overlapping mapped chords", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  t.translate(down("KeyC", true), true);
  // Second letter down before the first came up: one Control covers both, so no
  // second press and — crucially — no release until the last letter is up.
  assert.deepEqual(t.translate(down("KeyV", true), true), [sent("KeyV", true)]);
  assert.deepEqual(t.translate(up("KeyC", true), true), [sent("KeyC", false)]);
  assert.deepEqual(t.translate(up("KeyV", true), true), [
    sent("KeyV", false),
    sent("ControlLeft", false),
  ]);
});

test("translation off sends the physical codes untouched", () => {
  const t = new MacKeyboardTranslator();
  assert.deepEqual(t.translate(down("MetaLeft"), false), [
    sent("MetaLeft", true),
  ]);
  assert.deepEqual(t.translate(down("KeyV", true), false), [
    sent("KeyV", true),
  ]);
  assert.deepEqual(t.translate(up("KeyV", true), false), [sent("KeyV", false)]);
  assert.deepEqual(t.translate(up("MetaLeft"), false), [
    sent("MetaLeft", false),
  ]);
});

test("a bare Command tap presses and releases the remote's Windows key", () => {
  const t = new MacKeyboardTranslator();
  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("MetaLeft", true),
    sent("MetaLeft", false),
  ]);
});

// The SPA's own Ctrl+Cmd+Shift+; hides the floating menu and never reaches the
// input path, so the translator only ever sees the Command — which without this is
// indistinguishable from the bare tap above, and would open the guest's Start
// menu every time the toolbar was hidden. See FloatingMenu's hideChromeShortcut.
test("a chord the client took itself is not a bare Command tap", () => {
  const t = new MacKeyboardTranslator();
  assert.deepEqual(t.translate(down("ControlLeft"), true), [
    sent("ControlLeft", true),
  ]);
  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(down("ShiftLeft", true), true), [
    sent("ShiftLeft", true),
  ]);
  // The Semicolon never arrives; the toolbar reports the chord instead.
  t.noteCommandUsedLocally();
  assert.deepEqual(t.translate(up("MetaLeft"), true), []);
  assert.deepEqual(t.translate(up("ShiftLeft"), true), [
    sent("ShiftLeft", false),
  ]);
  assert.deepEqual(t.translate(up("ControlLeft"), true), [
    sent("ControlLeft", false),
  ]);
  // And the next real bare tap still taps: the flag went with the Command it was
  // set for, so a client shortcut cannot silently eat the one after it.
  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("MetaLeft", true),
    sent("MetaLeft", false),
  ]);
});

// Nothing is pending, so there is no Command for the report to be about. Setting
// the flag anyway would swallow the *next* tap instead of this non-existent one.
test("reporting a client chord with no Command held changes nothing", () => {
  const t = new MacKeyboardTranslator();
  t.noteCommandUsedLocally();
  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("MetaLeft", true),
    sent("MetaLeft", false),
  ]);
});

test("an unmapped Command chord forwards Command as itself", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  // KeyB is not in the table, so the guest gets the Meta chord rather than a
  // lone letter. This is also what any chord outside the eight does.
  assert.deepEqual(t.translate(down("KeyB", true), true), [
    sent("MetaLeft", true),
    sent("KeyB", true),
  ]);
  assert.deepEqual(t.translate(up("KeyB", true), true), [sent("KeyB", false)]);
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("MetaLeft", false),
  ]);
});

// The two modes — Command forwarded as itself, and Command replaced by a synthetic
// Control — are mutually exclusive, and one Command hold can ask for both. Holding
// them at once sends the guest a chord nobody typed.
test("switching from a forwarded chord to a mapped one takes Command back", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  assert.deepEqual(t.translate(down("KeyB", true), true), [
    sent("MetaLeft", true),
    sent("KeyB", true),
  ]);
  assert.deepEqual(t.translate(up("KeyB", true), true), [sent("KeyB", false)]);
  // Command never came up, so Meta is still down on the guest. Left there, this
  // would arrive as Meta plus Control plus C.
  assert.deepEqual(t.translate(down("KeyC", true), true), [
    sent("MetaLeft", false),
    sent("ControlLeft", true),
    sent("KeyC", true),
  ]);
  assert.deepEqual(t.translate(up("KeyC", true), true), [
    sent("KeyC", false),
    sent("ControlLeft", false),
  ]);
  // Command's own release adds nothing: the guest was told about that Meta and
  // then told to let it go, so there is nothing left owing.
  assert.deepEqual(t.translate(up("MetaLeft"), true), []);
});

test("switching from a mapped chord to a forwarded one drops the Control", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  assert.deepEqual(t.translate(down("KeyC", true), true), [
    sent("ControlLeft", true),
    sent("KeyC", true),
  ]);
  // KeyC is still held, so its synthetic Control is still down. Forwarding
  // Command on top of it would be the same invented chord the other way round.
  assert.deepEqual(t.translate(down("KeyB", true), true), [
    sent("ControlLeft", false),
    sent("MetaLeft", true),
    sent("KeyB", true),
  ]);
  assert.deepEqual(t.translate(up("KeyC", true), true), [sent("KeyC", false)]);
  assert.deepEqual(t.translate(up("KeyB", true), true), [sent("KeyB", false)]);
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("MetaLeft", false),
  ]);
});

// Command-Shift-Z is redo on a Windows guest, and the most ordinary chord that
// used to break: Shift looked like a key outside the table, so Command was
// forwarded and the guest got Meta-Shift-Control-Z.
test("a Shift on a mapped chord does not forward Command", () => {
  const t = new MacKeyboardTranslator();
  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(down("ShiftLeft", true), true), [
    sent("ShiftLeft", true),
  ]);
  assert.deepEqual(t.translate(down("KeyZ", true), true), [
    sent("ControlLeft", true),
    sent("KeyZ", true),
  ]);
  assert.deepEqual(t.translate(up("KeyZ", true), true), [
    sent("KeyZ", false),
    sent("ControlLeft", false),
  ]);
  assert.deepEqual(t.translate(up("ShiftLeft", true), true), [
    sent("ShiftLeft", false),
  ]);
  assert.deepEqual(t.translate(up("MetaLeft"), true), []);
});

test("a modifier held under Command is still not a chord of its own", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  // Option and Control pass through the same way, and none of them counts as the
  // key that would make this an unmapped chord.
  assert.deepEqual(t.translate(down("AltLeft", true), true), [
    sent("AltLeft", true),
  ]);
  assert.deepEqual(t.translate(down("ControlRight", true), true), [
    sent("ControlRight", true),
  ]);
  // So Command is still pending, and letting it go is still a bare tap.
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("MetaLeft", true),
    sent("MetaLeft", false),
  ]);
});

test("a bare Command tap still works after a forwarded chord", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  t.translate(down("KeyB", true), true);
  t.translate(up("KeyB", true), true);
  t.translate(up("MetaLeft"), true);

  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("MetaLeft", true),
    sent("MetaLeft", false),
  ]);
});

test("a bare Command tap still works after a translated chord", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  t.translate(down("KeyC", true), true);
  t.translate(up("KeyC", true), true);
  t.translate(up("MetaLeft"), true);

  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("MetaLeft", true),
    sent("MetaLeft", false),
  ]);
});

test("the two Command keys are tracked apart", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  t.translate(down("MetaRight"), true);
  // Releasing one of two held Commands is not a bare tap: the other is still
  // down, so nothing is sent yet.
  assert.deepEqual(t.translate(up("MetaLeft"), true), []);
  assert.deepEqual(t.translate(up("MetaRight"), true), [
    sent("MetaRight", true),
    sent("MetaRight", false),
  ]);
});

// The browser-specific half, and the reason this translator is not a straight
// copy of the viewer's: macOS browsers can withhold `keyup` for a character key
// while Command is held. Without the flush, the guest is left holding both the
// letter and a Control it can never be told to release.
test("a keyup swallowed under Command is flushed when Command comes up", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  assert.deepEqual(t.translate(down("KeyC", true), true), [
    sent("ControlLeft", true),
    sent("KeyC", true),
  ]);
  // No `up("KeyC")` at all — straight to Command's release.
  assert.deepEqual(t.translate(up("MetaLeft"), true), [
    sent("KeyC", false),
    sent("ControlLeft", false),
  ]);
});

test("a flush still lets the same chord be typed again", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  t.translate(down("KeyC", true), true);
  t.translate(up("MetaLeft"), true);

  t.translate(down("MetaLeft"), true);
  assert.deepEqual(t.translate(down("KeyC", true), true), [
    sent("ControlLeft", true),
    sent("KeyC", true),
  ]);
});

test("CapsLock is sent as a tap whichever way it was reported", () => {
  const t = new MacKeyboardTranslator();
  // Browsers report engaging it as a press and disengaging it as a release; the
  // guest wants a tap either way, and the caps flag rides on every message.
  assert.deepEqual(
    t.translate(
      { code: "CapsLock", pressed: true, caps: true, meta: false },
      true,
    ),
    [
      { code: "CapsLock", pressed: true, caps: true },
      { code: "CapsLock", pressed: false, caps: true },
    ],
  );
  assert.deepEqual(
    t.translate(
      { code: "CapsLock", pressed: false, caps: false, meta: false },
      true,
    ),
    [
      { code: "CapsLock", pressed: true, caps: false },
      { code: "CapsLock", pressed: false, caps: false },
    ],
  );
});

test("caps rides along on a translated chord", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  assert.deepEqual(
    t.translate({ code: "KeyC", pressed: true, caps: true, meta: true }, true),
    [
      { code: "ControlLeft", pressed: true, caps: true },
      { code: "KeyC", pressed: true, caps: true },
    ],
  );
});

test("reset emits nothing and leaves no chord half-open", () => {
  const t = new MacKeyboardTranslator();
  t.translate(down("MetaLeft"), true);
  t.translate(down("KeyC", true), true);
  // The caller has just swept its own pressed set; resuming the chord would send
  // releases for keys the guest has already been told about.
  t.reset();
  assert.deepEqual(t.translate(up("KeyC", true), true), [sent("KeyC", false)]);
  assert.deepEqual(t.translate(up("MetaLeft"), true), []);
});

test("a key with no Command involved is passed straight through", () => {
  const t = new MacKeyboardTranslator();
  assert.deepEqual(t.translate(down("KeyC"), true), [sent("KeyC", true)]);
  assert.deepEqual(t.translate(up("KeyC"), true), [sent("KeyC", false)]);
});

test("a browser-reserved Command chord is always mapped when it arrives", () => {
  const t = new MacKeyboardTranslator();
  assert.deepEqual(t.translate(down("MetaLeft"), true), []);
  assert.deepEqual(t.translate(down("KeyW", true), true), [
    sent("ControlLeft", true),
    sent("KeyW", true),
  ]);
  assert.deepEqual(t.translate(up("KeyW", true), true), [
    sent("KeyW", false),
    sent("ControlLeft", false),
  ]);
  assert.deepEqual(t.translate(up("MetaLeft"), true), []);
});
