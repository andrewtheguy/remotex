// The sender's frame-aligned motion coalescing, over a fake socket and a
// hand-cranked frame boundary. The congestion half (bufferedAmount + drain
// poll) rides real timers and a real WebSocket and stays browser QA; what is
// pinned here is the policy a machine's speed must not change: which moves are
// sent, which are dropped, and what order everything leaves in.
//
// Run with `bun test src/outbound.test.ts` from frontend/.
import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import { createSender } from "./outbound.ts";

interface FakeSocket {
  readyState: number;
  bufferedAmount: number;
  send(data: string): void;
}

let sent: { type: string; x?: number; y?: number }[] = [];
let frames: (() => void)[] = [];

const openSocket = (): FakeSocket => ({
  readyState: WebSocket.OPEN,
  bufferedAmount: 0,
  send(data: string) {
    sent.push(JSON.parse(data));
  },
});

/** Fire every pending frame callback, the way one rAF tick would. */
const tick = () => {
  const due = frames;
  frames = [];
  for (const frame of due) {
    frame();
  }
};

const sender = (ws: FakeSocket | null) =>
  createSender(
    () => ws as unknown as WebSocket | null,
    (frame) => {
      frames.push(frame);
    },
  );

beforeEach(() => {
  sent = [];
  frames = [];
});

test("the first move of a burst is never delayed", () => {
  const send = sender(openSocket());
  send({ type: "mouseMove", x: 1, y: 1 });
  assert.deepEqual(sent, [{ type: "mouseMove", x: 1, y: 1 }]);
});

test("moves within one frame collapse to the newest at the boundary", () => {
  const send = sender(openSocket());
  send({ type: "mouseMove", x: 1, y: 1 });
  send({ type: "mouseMove", x: 2, y: 2 });
  send({ type: "mouseMove", x: 3, y: 3 });
  assert.equal(sent.length, 1, "a same-frame move left before the boundary");
  tick();
  assert.deepEqual(sent.at(-1), { type: "mouseMove", x: 3, y: 3 });
  assert.equal(sent.length, 2, "an intermediate position was worth sending");
});

test("a quiet frame boundary sends nothing and costs nothing", () => {
  const send = sender(openSocket());
  send({ type: "mouseMove", x: 1, y: 1 });
  tick();
  assert.equal(sent.length, 1);
  assert.equal(frames.length, 0, "an idle sender kept a frame loop running");
});

test("steady per-frame motion goes out at full rate", () => {
  const send = sender(openSocket());
  for (let at = 1; at <= 3; at++) {
    send({ type: "mouseMove", x: at, y: at });
    tick();
  }
  assert.deepEqual(
    sent.map((m) => m.x),
    [1, 2, 3],
    "frame-rate motion is exactly what must not be thinned",
  );
});

test("anything that is not motion flushes the deferred move first", () => {
  // A click has to follow the move that positioned it: dropping the held move
  // would click in the wrong place.
  const send = sender(openSocket());
  send({ type: "mouseMove", x: 1, y: 1 });
  send({ type: "mouseMove", x: 2, y: 2 });
  send({ type: "mouseButton", button: "left", pressed: true, clicks: 1 });
  assert.deepEqual(
    sent.map((m) => [m.type, m.x]),
    [
      ["mouseMove", 1],
      ["mouseMove", 2],
      ["mouseButton", undefined],
    ],
  );
});

test("a move deferred on one socket is not replayed onto the next", () => {
  const first = openSocket();
  let current: FakeSocket | null = first;
  const send = createSender(
    () => current as unknown as WebSocket | null,
    (frame) => {
      frames.push(frame);
    },
  );
  send({ type: "mouseMove", x: 1, y: 1 });
  send({ type: "mouseMove", x: 2, y: 2 });
  current = openSocket(); // reconnect before the boundary
  tick();
  assert.equal(
    sent.length,
    1,
    "a coordinate from the old attachment reached the new one",
  );
});

test("a backed-up socket defers, and the newest goes when it drains", async () => {
  // Left on the real drain poll deliberately: resolving the deferral is what
  // stops the poll, so the test must see it through rather than abandon a
  // timer chain that would outlive it.
  const ws = openSocket();
  const send = sender(ws);
  ws.bufferedAmount = 10;
  send({ type: "mouseMove", x: 1, y: 1 });
  send({ type: "mouseMove", x: 2, y: 2 });
  assert.deepEqual(sent, [], "a move was sent into a congested socket");
  ws.bufferedAmount = 0;
  for (let waited = 0; waited < 50 && sent.length === 0; waited++) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.deepEqual(sent, [{ type: "mouseMove", x: 2, y: 2 }]);
});
