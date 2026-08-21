// The touchscreen forwarder's slot policy, driven without a document: which
// contact ids go out for which fingers, what an eleventh finger gets, and that
// a release cancels exactly what is still down.
//
// Run with `bun test src/touchPassthrough.test.ts` from frontend/.
import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import type { ClientMsg } from "./protocol.ts";
import {
  createTouchForwarder,
  MAX_CONTACTS,
  type TouchForwarder,
} from "./touchPassthrough.ts";

type TouchMsg = Extract<ClientMsg, { type: "touch" }>;

let sent: TouchMsg[] = [];
let forwarder: TouchForwarder;
let framebuffer: { w: number; h: number } | null = { w: 1000, h: 500 };

// The framebuffer is twice the client's size, with a (10, 20) origin: a
// mapping shaped like the real one — scale and offset, clamped.
const toRemote = (clientX: number, clientY: number) => {
  if (!framebuffer) {
    return null;
  }
  const x = Math.min(Math.max(Math.round((clientX - 10) * 2), 0), 999);
  const y = Math.min(Math.max(Math.round((clientY - 20) * 2), 0), 499);
  return { x, y };
};

const finger = (identifier: number, clientX: number, clientY: number) => [
  { identifier, clientX, clientY },
];

beforeEach(() => {
  sent = [];
  framebuffer = { w: 1000, h: 500 };
  forwarder = createTouchForwarder({
    send: (msg) => {
      if (msg.type === "touch") {
        sent.push(msg);
      }
    },
    toRemote,
  });
});

test("a finger is named by the lowest free slot, down to up", () => {
  forwarder.down(finger(7_000_001, 110, 70));
  forwarder.move(finger(7_000_001, 120, 70));
  forwarder.up(finger(7_000_001, 120, 70));
  assert.deepEqual(sent, [
    { type: "touch", id: 1, phase: "down", x: 200, y: 100 },
    { type: "touch", id: 1, phase: "move", x: 220, y: 100 },
    { type: "touch", id: 1, phase: "up", x: 220, y: 100 },
  ]);
  assert.equal(forwarder.held(), 0);
});

test("two fingers get two slots, and a lifted slot is reused", () => {
  forwarder.down(finger(1, 10, 20));
  forwarder.down(finger(2, 20, 20));
  forwarder.up(finger(1, 10, 20));
  forwarder.down(finger(3, 30, 20));
  assert.deepEqual(
    sent.map((m) => [m.id, m.phase]),
    [
      [1, "down"],
      [2, "down"],
      [1, "up"],
      [1, "down"],
    ],
  );
});

test("an eleventh finger is ignored from its down to its up", () => {
  for (let i = 1; i <= MAX_CONTACTS + 1; i += 1) {
    forwarder.down(finger(i, 10 + i, 20));
  }
  assert.equal(forwarder.held(), MAX_CONTACTS);
  sent = [];
  forwarder.move(finger(MAX_CONTACTS + 1, 50, 50));
  forwarder.up(finger(MAX_CONTACTS + 1, 50, 50));
  assert.deepEqual(sent, []);
});

test("a move or up for a finger never put down is dropped", () => {
  forwarder.move(finger(9, 50, 50));
  forwarder.up(finger(9, 50, 50));
  forwarder.cancel(finger(9, 50, 50));
  assert.deepEqual(sent, []);
});

test("before the first resize there is nothing to map onto, so no contact", () => {
  framebuffer = null;
  forwarder.down(finger(1, 50, 50));
  assert.deepEqual(sent, []);
  assert.equal(forwarder.held(), 0);
});

test("positions are clamped to the framebuffer, like the mouse", () => {
  forwarder.down(finger(1, 0, 0));
  forwarder.move(finger(1, 5000, 5000));
  assert.deepEqual(
    sent.map((m) => [m.x, m.y]),
    [
      [0, 0],
      [999, 499],
    ],
  );
});

test("release cancels what is still down, where it last was, and only that", () => {
  forwarder.down(finger(1, 10, 20));
  forwarder.down(finger(2, 60, 70));
  forwarder.move(finger(2, 70, 70));
  forwarder.up(finger(1, 10, 20));
  sent = [];
  forwarder.release();
  assert.deepEqual(sent, [
    { type: "touch", id: 2, phase: "cancel", x: 120, y: 100 },
  ]);
  assert.equal(forwarder.held(), 0);
  forwarder.release();
  assert.equal(sent.length, 1, "a second release has nothing to cancel");
});

test("a cancel frees the slot like an up", () => {
  forwarder.down(finger(1, 10, 20));
  forwarder.cancel(finger(1, 10, 20));
  forwarder.down(finger(2, 10, 20));
  assert.deepEqual(
    sent.map((m) => [m.id, m.phase]),
    [
      [1, "down"],
      [1, "cancel"],
      [1, "down"],
    ],
  );
});
