import type { ClientMsg } from "./protocol.ts";

// WebSocket.OPEN is specified as 1. Keeping the value here lets the lifecycle
// gate run in the Node unit test without inventing a browser WebSocket global.
const SOCKET_OPEN = 1;

export interface PaintGeneration {
  current: number;
}

export interface PaintAckSocket {
  readonly readyState: number;
  send(data: string): void;
}

/** Advance the component-owned epoch for an attachment start or teardown. */
export function advancePaintGeneration(generation: PaintGeneration): number {
  generation.current += 1;
  return generation.current;
}

/** Send completion only to the live socket that owns this worker command. */
export function sendPaintAck(
  currentGeneration: PaintGeneration,
  generation: number,
  attachmentSocket: PaintAckSocket | null,
  liveSocket: PaintAckSocket | null,
  sequence: number,
  queuedMs: number,
  drawMs: number,
): boolean {
  if (
    generation !== currentGeneration.current ||
    attachmentSocket === null ||
    attachmentSocket !== liveSocket ||
    attachmentSocket.readyState !== SOCKET_OPEN
  ) {
    return false;
  }
  const message = {
    type: "paintAck",
    sequence,
    queuedMs,
    drawMs,
  } satisfies ClientMsg;
  attachmentSocket.send(JSON.stringify(message));
  return true;
}
