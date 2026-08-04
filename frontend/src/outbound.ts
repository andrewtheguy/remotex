import type { ClientMsg } from "./protocol.ts";

/// How often a deferred pointer move rechecks whether the socket has drained.
///
/// A poll rather than an event, because a WebSocket has none for
/// `bufferedAmount` reaching zero. 8 ms is about one frame on a fast display, so
/// a drag resumes full rate within a frame of the socket catching up. Background
/// tabs clamp timers to a second, which costs nothing: a tab nobody is pointing
/// at has no motion to coalesce, and the last move still arrives.
const DRAIN_POLL_MS = 8;

type PointerMove = Extract<ClientMsg, { type: "mouseMove" }>;

/// Every client message leaves through here, and pointer motion is the only kind
/// that is ever held back. Two things hold it:
///
/// - **The display's frame rate.** Nothing downstream can show more than one
///   position per painted frame, and some devices report motion faster than
///   they paint, so after one move has left in a frame, later ones replace a
///   deferred newest that goes at the next frame boundary. The first move of a
///   burst is never delayed.
/// - **Congestion.** A fast drag can produce motion faster than the link
///   carries it, and a stale coordinate is worthless the moment a newer one
///   exists — so while the socket has bytes still queued, only the newest move
///   is kept, polled out when the socket drains.
///
/// Two rules make the holding safe:
///
/// - **Only motion is coalesced.** Anything else flushes the deferred move
///   first, because a click has to follow the move that positioned it. Dropping
///   a move that a later message depends on would click in the wrong place.
/// - **A deferred move belongs to the socket it was made on.** A reconnect
///   discards it rather than replaying it, since a coordinate from the previous
///   attachment is a pointer jump the new session never asked for.
///
/// `schedule` is the frame boundary, injectable for tests; motion only happens
/// in a foreground tab, so a boundary that never fires has nothing deferred
/// behind it worth more than the flush the next non-motion message performs.
export function createSender(
  socket: () => WebSocket | null,
  schedule: (frame: () => void) => void = (frame) => {
    requestAnimationFrame(frame);
  },
) {
  let deferred: { move: PointerMove; ws: WebSocket } | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let frameArmed = false;
  let sentThisFrame = false;

  const open = (ws: WebSocket | null): ws is WebSocket =>
    ws !== null && ws.readyState === WebSocket.OPEN;

  const flush = () => {
    const held = deferred;
    deferred = null;
    const ws = socket();
    if (held && ws === held.ws && open(ws)) {
      ws.send(JSON.stringify(held.move));
      sentThisFrame = true;
      armFrame();
    }
  };

  // One callback per frame boundary: reopen the frame's send budget, and spend
  // it at once on a move deferred by the previous frame — unless the socket has
  // backed up meanwhile, in which case the drain poll takes over.
  const armFrame = () => {
    if (frameArmed) {
      return;
    }
    frameArmed = true;
    schedule(() => {
      frameArmed = false;
      sentThisFrame = false;
      if (!deferred) {
        return;
      }
      const ws = socket();
      if (ws === deferred.ws && open(ws) && ws.bufferedAmount > 0) {
        arm();
        return;
      }
      flush();
    });
  };

  const poll = () => {
    timer = undefined;
    if (!deferred) {
      return;
    }
    const ws = socket();
    // Anything but "same socket, still open, still backed up" resolves the
    // deferred move now — either by sending it or, if its socket is gone, by
    // dropping it in `flush`.
    if (ws === deferred.ws && open(ws) && ws.bufferedAmount > 0) {
      arm();
      return;
    }
    flush();
  };

  const arm = () => {
    if (timer === undefined) {
      timer = setTimeout(poll, DRAIN_POLL_MS);
    }
  };

  return (msg: ClientMsg) => {
    const ws = socket();
    if (!open(ws)) {
      deferred = null;
      return;
    }
    if (msg.type !== "mouseMove") {
      flush();
      ws.send(JSON.stringify(msg));
      return;
    }
    // The frame budget is checked first: right after a send, `bufferedAmount`
    // still counts the sent bytes, so testing it first routed every same-frame
    // move to the 8 ms drain poll — which can fire inside the frame and send a
    // second position. The frame callback hands off to the poll itself when
    // the socket is still backed up at the boundary.
    if (sentThisFrame) {
      deferred = { move: msg, ws };
      armFrame();
      return;
    }
    if (ws.bufferedAmount > 0) {
      deferred = { move: msg, ws };
      arm();
      return;
    }
    deferred = null;
    ws.send(JSON.stringify(msg));
    sentThisFrame = true;
    armFrame();
  };
}
