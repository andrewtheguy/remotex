# What Guacamole does that this gateway does not

A source-level comparison against guacamole-server 1.6.1, made 2026-08-05, after the
FreeRDP engine swap improved RDP but left a felt gap against guacd on the same kind
of link. This is a record of *mechanisms*, each checked in Guacamole's source rather
than inferred from its reputation, with the remotex counterpart named beside it.
File references are into guacamole-server 1.6.1; the reference checkouts live under
`tmp/programs_for_reference/`, which is not committed — the version pin is what makes
the references reproducible.

It is a measurement backlog, not a plan: each gap below is worth taking only with a
number attached, the same way `video`'s 33 ms cap and the paint window were taken.
The list at the end is ordered by expected value against expected effort.

## Where the two agree

Worth recording so the gaps are read as gaps and not as a rewrite proposal. Both
sides: hardware cursor drawn by the client, with server cursor shapes forwarded and
never composited into the framebuffer; pixel-exact diffing against a copy of what
the client holds (guacd's 64×64 cell memcmp, this gateway's per-row span narrowing
in `Shadow::accept`); encoding off the protocol thread on a worker pool; `LAN`
declared instead of measured, for the same reason discovered independently; and
client feedback that paces the sender — guacd's `sync` round-trip, this gateway's
`paintAck` window, and the paintAck carries more (per-batch `queuedMs`/`drawMs`)
than guacd's single lag number. Mouse-move coalescing this gateway does client-side
per animation frame; guacd does none at all and sends every move.

## The gaps

### Frame boundaries: markers versus a timer

guacd asks the server to say where frames end — `FreeRDP_FrameMarkerCommandEnabled`
and `FreeRDP_SurfaceFrameMarkerEnabled` (`settings.c:1578`) — and flushes on the
FRAME_END event (`gdi.c:38-71`), acknowledging each frame back when the server
negotiated `FrameAcknowledge` so its flow-control window stays open. The render
thread only falls back to a timer (100 ms) when no marker arrives.

This gateway sets neither and reconstructs boundaries with the 16 ms damage
coalescer (`DAMAGE_INTERVAL`, `src/rdp.rs`) under the 33 ms video cap. That is a
guess where a fact is available: the guess adds up to 16 ms to every update and can
cut a multi-rect frame in half. The wrapper would surface FRAME_END as an event
beside `Paint`.

### Performance flags: damage that never has to exist

`guac_rdp_get_performance_flags` (`settings.c:1489-1519`) disables wallpaper,
theming, full-window drag and menu animations by default, and sets the individual
`Disable*` booleans redundantly because some FreeRDP versions overwrite the flags.
This gateway sets only `DisableMenuAnims`, and keeps full-window drag **on** — which
prices every window drag at a full window repaint per position, through damage,
encode, socket, decode and paint. Turning drag off is the single cheapest
damage-volume lever the comparison found. `BitmapCacheEnabled` and
`OffscreenSupportLevel` guacd also enables on the legacy path; glyph caching it
forces off regardless of settings, for upstream instability (GUACAMOLE-1191).

### Adaptive quality, per update rather than per session

guacd re-decides codec and quality for every update. Quality tracks the measured
client lag linearly — `quality = clamp(90 − (lag − 20), 30, 90)`
(`display-worker.c:135-152`). Codec tracks content: each 64×64 cell carries the
timestamp of its last change, cells updating at ≥ 3 fps go lossy (JPEG/WebP), and a
PNG-optimality estimator — adjacent-identical versus adjacent-different pixels, a
proxy for DEFLATE-compressibility — keeps text and flat UI lossless
(`display-worker.c:171-221`). Its PNG encoder palettizes ≤ 256-colour regions down
to 1/2/4-bit indexed.

This gateway's still-tile paths take one codec and one quality from config for the
session, default lossless PNG, and only the video paths have the congestion dial.
The lag signal the quality curve needs is already collected — the paint window's
`behind()` is exactly guacd's `processing_lag` — it just gates the send window
instead of moving quality.

### Copy detection: scrolling without image bytes

guacd hashes every dirty 64×64 cell of the pending frame into a 65 536-bucket index,
scans the previous frame across the damaged region, byte-verifies hash hits, and
rewrites matches as `copy` instructions from the client's own canvas
(`display-plan-search.c`). A scrolled page becomes copy ops and near-zero image
bytes. This gateway has `OP_COPY` on the wire, a client that executes it, and
supersede rules that already respect it — but only VNC CopyRect ever emits one; the
RDP path re-encodes every scroll. The shadow already holds the previous frame, so
the search has its two operands in hand.

### The graphics pipeline, and what guacd's working EGFX says about the black screen

guacd runs EGFX by default: `SupportGraphicsPipeline` TRUE with `RemoteFxCodec` TRUE
beside it and `ColorDepth` forced to 32 (`settings.c:1588-1598`) — and *only* those;
H.264, AVC444 and progressive are never touched and default off. Decoding still ends
in FreeRDP's software GDI and the same primary buffer this gateway reads, so guacd's
Windows 11 sessions are the same consumption model with the pipeline on.

That sharpens the black-framebuffer finding recorded in [`roadmap.md`](roadmap.md):
an implementation with near-identical GDI setup has the pipeline working against the
same Windows generation, so the failure is likely a small settings delta — the
codec set advertised beside the pipeline, or frame acknowledgement — rather than
anything structural. Worth one retry with guacd's exact combination before the
`gdi_OutputUpdate` instrumentation the roadmap already plans.

### Transport details

- guacd sets `TCP_NODELAY` on every accepted connection (`guacd/daemon.c:563`),
  naming Nagle as the reason. This gateway sets it on the VNC-to-host socket only;
  the axum listener's browser-facing sockets are left at the OS default, in front
  of an ack-gated window where a delayed segment is a stalled window.
- guacd buffers writes 8 KB deep and flushes exactly at frame boundaries; its
  WebSocket tunnel batches until it is about to block, never on a timer. The same
  shape as `wire.rs`'s drain-don't-wait batching — parity, recorded because the
  agreement is evidence the shape is right.
- guacd deliberately does not flush a frame for a mouse-position-only change,
  having measured slowdowns from the sheer quantity of frames.

### Backpressure by merging rather than queueing

guacd holds at most one frame in flight: if the previous frame is still encoding,
the new one is not queued — its damage keeps accumulating into the pending state,
and the last worker to finish re-triggers the flush (`display-flush.c:316`). Ahead
of that, the render thread sleeps up to 500 ms of measured client processing lag so
that a slow client makes frames merge server-side (`display-render-thread.c`). The
merge count travels to the client in `sync`, so drops are observable. This gateway
gets a similar effect from the shallow video queues plus the paint window, but the
still-tile paths can buffer ~144 records across the queues in series — a depth
[`roadmap.md`](roadmap.md) records as deliberately left alone because the paint
window now bounds what matters. The difference in *shape* — merge-at-source versus
bound-at-sink — is worth remembering when a new symptom appears.

## In order of expected value against effort

1. `TCP_NODELAY` on the browser socket — a line in the accept path, measurable with
   the paintAck lag numbers already recorded.
2. Performance flags: full-window drag off above all, wallpaper and theming behind
   it. Damage that is never created needs no other optimization.
3. Frame markers as the flush signal, with the coalescer kept as the fallback.
4. Lag-adaptive quality on the still-tile paths, from the paint window's existing
   `behind()`.
5. Copy detection over the shadow for the RDP path — the scrolling win.
6. Per-tile content-aware codec choice (the PNG-optimality estimator).
7. The EGFX retry with guacd's exact settings, ahead of the roadmap's
   instrumentation plan.
8. Explicit bitmap/offscreen cache flags on the legacy path.

The audio half of what this comparison session found — RDP sound dying after a
resize — was a bug, not a gap, and is recorded in
[`rdp-audio-prior-art.md`](rdp-audio-prior-art.md).
