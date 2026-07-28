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
/// that is ever held back.
///
/// A fast drag can produce motion faster than the link carries it, and a stale
/// coordinate is worthless the moment a newer one exists — so while the socket
/// has bytes still queued, only the newest move is kept and the rest are dropped.
/// While the socket is keeping up, which is the normal case, nothing is deferred
/// at all: the gate is `bufferedAmount`, so this engages under congestion and
/// stays out of the way otherwise.
///
/// Two rules make that safe, and they are the same two the native viewer's
/// `OutboundQueue` follows:
///
/// - **Only motion is coalesced.** Anything else flushes the deferred move
///   first, because a click has to follow the move that positioned it. Dropping
///   a move that a later message depends on would click in the wrong place.
/// - **A deferred move belongs to the socket it was made on.** A reconnect
///   discards it rather than replaying it, since a coordinate from the previous
///   attachment is a pointer jump the new session never asked for.
export function createSender(socket: () => WebSocket | null) {
  let deferred: { move: PointerMove; ws: WebSocket } | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const open = (ws: WebSocket | null): ws is WebSocket =>
    ws !== null && ws.readyState === WebSocket.OPEN;

  const flush = () => {
    const held = deferred;
    deferred = null;
    const ws = socket();
    if (held && ws === held.ws && open(ws)) {
      ws.send(JSON.stringify(held.move));
    }
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
    if (ws.bufferedAmount === 0) {
      deferred = null;
      ws.send(JSON.stringify(msg));
      return;
    }
    deferred = { move: msg, ws };
    arm();
  };
}
