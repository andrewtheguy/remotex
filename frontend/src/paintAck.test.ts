import assert from "node:assert/strict";
import { test } from "node:test";
import {
  advancePaintGeneration,
  type PaintAckSocket,
  sendPaintAck,
} from "./paintAck.ts";

function socket(): PaintAckSocket & { sent: string[] } {
  return {
    readyState: 1,
    sent: [],
    send(data) {
      this.sent.push(data);
    },
  };
}

test("an effect rerun on one canvas cannot ack its old attachment", () => {
  // The ref represents component scope. The first effect queues a worker draw,
  // then its cleanup and the replacement effect both advance the same counter.
  const currentGeneration = { current: 0 };
  const oldSocket = socket();
  const oldGeneration = advancePaintGeneration(currentGeneration);
  advancePaintGeneration(currentGeneration); // first effect's cleanup
  const newSocket = socket();
  const newGeneration = advancePaintGeneration(currentGeneration);

  // desktopPainter has rebound its handler to the new effect, but the event it
  // delivers still echoes the old command's generation.
  assert.equal(
    sendPaintAck(
      currentGeneration,
      oldGeneration,
      newSocket,
      newSocket,
      1,
      7,
      11,
    ),
    false,
  );
  assert.deepEqual(newSocket.sent, []);

  assert.equal(
    sendPaintAck(
      currentGeneration,
      newGeneration,
      newSocket,
      newSocket,
      1,
      3,
      5,
    ),
    true,
  );
  assert.deepEqual(newSocket.sent, [
    '{"type":"paintAck","sequence":1,"queuedMs":3,"drawMs":5}',
  ]);
  assert.deepEqual(oldSocket.sent, []);
});
