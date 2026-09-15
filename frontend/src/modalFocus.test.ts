// Tab wrapping inside a modal card, over fake elements: which element takes focus,
// and whether the browser's own move was cancelled.
import assert from "node:assert/strict";
import { test } from "node:test";
import { type Focusable, keepTabWithin, type ModalCard } from "./modalFocus.ts";

let focused: unknown = null;

function element(): Focusable {
  const el: Focusable = {
    focus() {
      focused = el;
    },
  };
  return el;
}

function card(controls: Focusable[]): ModalCard {
  const c: ModalCard = {
    focus() {
      focused = c;
    },
    contains: (node) => node === c || controls.includes(node as Focusable),
    querySelectorAll: () => controls,
  };
  return c;
}

function press(shiftKey: boolean, from: unknown, within: ModalCard) {
  focused = from;
  let prevented = false;
  keepTabWithin(within, from, {
    shiftKey,
    preventDefault() {
      prevented = true;
    },
  });
  return prevented;
}

const first = element();
const middle = element();
const last = element();
const modal = card([first, middle, last]);
const background = element();

test("Shift+Tab from the card itself, as it opens, wraps to the last control", () => {
  assert.equal(press(true, modal, modal), true);
  assert.equal(focused, last);
});

test("Tab from the card itself goes to the first control", () => {
  assert.equal(press(false, modal, modal), true);
  assert.equal(focused, first);
});

test("Shift+Tab from the first control wraps to the last", () => {
  assert.equal(press(true, first, modal), true);
  assert.equal(focused, last);
});

test("Tab from the last control wraps to the first", () => {
  assert.equal(press(false, last, modal), true);
  assert.equal(focused, first);
});

test("focus outside the card is brought back in", () => {
  assert.equal(press(false, background, modal), true);
  assert.equal(focused, first);
  assert.equal(press(true, background, modal), true);
  assert.equal(focused, last);
});

test("between controls the browser moves focus itself", () => {
  assert.equal(press(false, middle, modal), false);
  assert.equal(press(true, middle, modal), false);
  assert.equal(focused, middle);
});

test("a card with no controls keeps focus on itself", () => {
  const empty = card([]);
  assert.equal(press(true, empty, empty), true);
  assert.equal(focused, empty);
});
