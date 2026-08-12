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

### Frame boundaries — taken

guacd asks the server to say where frames end — `FreeRDP_FrameMarkerCommandEnabled`
and `FreeRDP_SurfaceFrameMarkerEnabled` (`settings.c:1578`) — and flushes on the
FRAME_END event (`gdi.c:38-71`), acknowledging each frame back when the server
negotiated `FrameAcknowledge` so its flow-control window stays open. The render
thread only falls back to a timer (100 ms) when no marker arrives.

This gateway used to set neither and reconstruct boundaries with the 16 ms damage
coalescer (`DAMAGE_INTERVAL`, `src/rdp.rs`) — a guess where a fact was available,
adding up to 16 ms to every update and able to cut a multi-rect frame in half.
**Now the wrapper requests both markers and surfaces the END as `Event::Frame`**,
acknowledging the surface flavour the way guacd does; EGFX sessions need no marker
because the pipeline flushes its surfaces once per frame PDU, and the wrapper marks
that flush. On the first `Frame` a server ever sends, `src/rdp.rs` switches
regimes: the marker becomes the flush signal and the timer demotes to a 100 ms
safety net (`FRAME_NET`, guacd's own fallback number). A server that never marks
keeps the original coalescer. Measured with `freerdp-e2e`: both kinds of server
marked 4 boundaries across 5 paints, the expected shape.

That measurement was taken on the EGFX path, and the caches below later showed it
does not carry to the other one. The ALTSEC frame marker is itself an order, so
it cannot arrive on a connection sending none: the same Windows 11 host, run with
`egfx = false` and no bitmap cache, marked **nothing at all**, and began marking
only once the caches gave it a reason to send orders.

Orders turn out to be necessary but not sufficient, and this is where the claim
above — that no never-marking server had been seen — was simply untested rather
than true. xrdp on the legacy path marks nothing in any run, with the caches or
without, while sending 960 MemBlt orders. So `FRAME_NET` and the 16 ms coalescer
are live code on that pairing rather than a theoretical fallback, and the regime
switch in `src/rdp.rs` is doing real work in deciding per server rather than
per configuration.

### Performance flags — taken

`guac_rdp_get_performance_flags` (`settings.c:1489-1519`) disables wallpaper,
theming, full-window drag and menu animations by default, and sets the individual
`Disable*` booleans redundantly because some FreeRDP versions overwrite the flags.
This gateway set only `DisableMenuAnims` and kept full-window drag **on** — pricing
every window drag at a full window repaint per position, through damage, encode,
socket, decode and paint. **The wrapper now ships guacd's defaults**: wallpaper,
theming, drag and menu animations all off, reversing the recorded drag-stays-on
decision (the reversal and its price are in the wrapper's comment). Only the
booleans are set, because the pinned FreeRDP derives the wire value from them
(`freerdp_performance_flags_make`) as the info packet is written — the redundant
uint32 guacd also sets guards older FreeRDPs this build does not link.

### The legacy path's caches — taken

`BitmapCacheEnabled` and `OffscreenSupportLevel`, which guacd also enables
(`settings.c:1730-1731`), are now set in the wrapper too. FreeRDP 3 ships both
off — `freerdp_settings_new` leaves the bitmap key zeroed, reachable only through
a Windows registry hook, and sets the offscreen level to 0 — so every session
before this ran the legacy orders path with no client-side cache at all. Glyph
caching stays off: guacd forces it off regardless of settings for upstream
instability (GUACAMOLE-1191), and FreeRDP's own settings warning calls a non-NONE
level experimental.

The effect is larger than "some blits get cheaper". Measured against the Windows
11 host with `egfx = false`, twice each way, reading FreeRDP's end-of-session
`update_dump_stats`: **without** the caches that host sent no drawing orders
whatsoever — 71 and 72 raw bitmap updates, every one of them pixels. **With**
them it sent no raw bitmap update at all: 1941 and 1945 MemBlt orders drawn out
of 574 and 587 cached bitmaps, so roughly two of every three blits repainted from
memory the client already held. Denying a server the cache does not make it send
the same picture more cheaply some other way; it makes it stop using orders and
push pixels. The same counters print from a gateway — `WLOG_LEVEL=TRACE` against
an `egfx = false` target — which is where a third reading of 2922 MemBlt over 152
cached bitmaps came from.

xrdp is the check that this is not one vendor's quirk, and it gives the same
shape from a much quieter desktop: 0 orders and 5 raw bitmap updates without the
caches, 960 MemBlt orders over 156 and 337 cached bitmaps and no raw update at
all with them. What moves is the server's choice of mechanism, not the amount it
had to draw.

Order counts are mechanism and not volume, so whether the new mechanism is
*cheaper* is a separate measurement: inbound bytes on the socket to xrdp over
fixed 25-second windows held by a gateway on an `egfx = false` target, with the
arms interleaved on/off/on/off so a drifting desktop could not land on one of
them.

| workload | with caches | without | |
| --- | --- | --- | --- |
| static desktop | ~1.6 KB | ~1.0 KB | nothing to be better at |
| video in a window | 104 MB | 121 MB | −14%, arms not overlapping |
| full-screen video | 279 MB | 301 MB | −7%, all four pairs |

Full-screen video is the case with a reason to regress — nearly every pixel is
novel, and each tile now carries an order as well — and it did not; all six pairs
across both video workloads fell the same way. That is a byte rate rather than a
per-frame efficiency claim, because the uncached arm painted slightly more frames
downstream and this instrument cannot separate that from cheaper ones. The static
desktop is the honest shape of the win: a cache pays where content repeats, and
an idle screen transfers nothing to save.

Frame markers came with it on the Windows host, which is the correction recorded
above; xrdp sends orders and still marks nothing. Both settings
are unconditional rather than gated on `egfx`, because they are capabilities of
the orders channel — what an EGFX-off session speaks, and what an EGFX-on session
falls back to when a server declines the pipeline. A server driving EGFX ignores
them.

### Adaptive quality, per update rather than per session — taken, as an option

guacd re-decides codec and quality for every update. Quality tracks the measured
client lag linearly — `quality = clamp(90 − (lag − 20), 30, 90)`
(`display-worker.c:135-152`). Codec tracks content: each 64×64 cell carries the
timestamp of its last change, cells updating at ≥ 3 fps go lossy (JPEG/WebP), and a
PNG-optimality estimator — adjacent-identical versus adjacent-different pixels, a
proxy for DEFLATE-compressibility — keeps text and flat UI lossless
(`display-worker.c:171-221`). Its PNG encoder palettizes ≤ 256-colour regions down
to 1/2/4-bit indexed.

**The quality half is now `render_adaptive`**, per target and off by default where
guacd's is always on. The lag signal is the one this table predicted: the paint
window's `behind()`, published through `src/feedback.rs` minus a windowed-min
baseline (RustDesk's move, so distance never reads as queueing). Lossy tiles take
guacd's exact curve per encode — one point per ms past 20 ms, floored at
`render_adaptive_min` instead of a hardcoded 30 — and the video/stream paths feed
the same lag into their existing congestion walk as a second down-trigger beside
push-blocking, with the same configurable floor. The audio counterpart
(`audio_adaptive`: an Opus bitrate walk on the audio socket's own backpressure,
plus shedding silent wave buffers while behind) has no guacd analogue at all —
guacd carries only raw PCM. The codec-tracks-content half (the PNG-optimality
estimator, item 6) remains untaken.

### Copy detection — taken

guacd hashes every dirty 64×64 cell of the pending frame into a 65 536-bucket index,
scans the previous frame across the damaged region, byte-verifies hash hits, and
rewrites matches as `copy` instructions from the client's own canvas
(`display-plan-search.c`). A scrolled page becomes copy ops and near-zero image
bytes. This gateway had `OP_COPY` on the wire, a client that executes it, and
supersede rules that already respect it — but only VNC CopyRect ever emitted one.

**`src/copies.rs` now runs the same search over the shadow** on every RDP damage
flush whose plan takes copies, with one structural improvement over the original:
because `Shadow::copy_within` applies each copy exactly as the client does and the
tile pass that follows repaints whatever diverged, a wrong copy can only waste,
never corrupt — so the byte-verification is an economy, not a safety. Matched cells
merge per displacement into few records, ordered so overlapping copies read their
sources first. Measured: a worst-case full-HD single-frame scroll plans in 16.5 ms
release; live against the Windows 11 host, thirty wheel steps over an Explorer
window sent 125 copy records moving 3,497,984 pixels that previously traveled as
PNG.

### The graphics pipeline — taken, and the black screen explained

guacd runs EGFX by default: `SupportGraphicsPipeline` TRUE with `RemoteFxCodec` TRUE
beside it and `ColorDepth` forced to 32 (`settings.c:1588-1598`) — and *only* those;
H.264, AVC444 and progressive are never touched and default off. Decoding still ends
in FreeRDP's software GDI and the same primary buffer this gateway reads, so guacd's
Windows 11 sessions are the same consumption model with the pipeline on.

**This one was retried and it was the fix.** The black-framebuffer fault that kept
the pipeline off here was the pipeline advertised *without a codec beside it*; with
guacd's exact pair, the e2e that had measured a framebuffer summing to exactly black
measured 3,090,403 non-zero bytes of 3,145,728 against the same host. The wrapper
ships the pair by default, independently of resize. An EGFX resize is a graphics
reset: no reactivation or reconnect, and sound survives, at the cost of a Windows
host's text staying soft. The `egfx = false` opt-out takes the sharp legacy path,
where each resize is a reactivation. A Windows host's audio redirector does not
survive that event, so a legacy session whose sound negotiated on the dynamic
`rdpsnd` transport resizes the way guacamole's `resize-method: reconnect` does.
The whole story is in
[`architecture.md`](architecture.md)'s RDP section.

### Transport details

- guacd sets `TCP_NODELAY` on every accepted connection (`guacd/daemon.c:563`),
  naming Nagle as the reason. **Taken**: `NodelayListener` (`src/server.rs`) sets
  it on every socket the gateway accepts, in both the served and embedded shapes;
  it was previously only on the VNC-to-host socket, leaving the browser-facing
  side — in front of an ack-gated window where a delayed segment is a stalled
  window — at the OS default.
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

1. ~~`TCP_NODELAY` on the browser socket~~ — **done**, `NodelayListener` in
   `src/server.rs`.
2. ~~Performance flags: full-window drag off above all, wallpaper and theming
   behind it~~ — **done**, in the wrapper, as guacd's defaults.
3. ~~Frame markers as the flush signal, with the coalescer kept as the fallback~~ —
   **done**: `Event::Frame` from the wrapper, marker-or-net flush in `src/rdp.rs`.
4. ~~Lag-adaptive quality on the still-tile paths, from the paint window's existing
   `behind()`~~ — **done**, `render_adaptive` in `src/config.rs`: guacd's curve on
   the still tiles, the lag signal into the streaming walk, and an Opus walk beside
   them that guacd never had.
5. ~~Copy detection over the shadow for the RDP path~~ — **done**, `src/copies.rs`;
   125 records moving 3.5M pixels in the first live scroll it saw.
6. Per-tile content-aware codec choice (the PNG-optimality estimator).
7. ~~The EGFX retry with guacd's exact settings~~ — **done**; it was the black
   screen's cause and the fix for Windows resize audio besides.
8. ~~Explicit bitmap/offscreen cache flags on the legacy path~~ — **done**, in the
   wrapper; it turned out to decide whether that path uses drawing orders at all,
   and on Windows brought that path's frame markers with it.

The audio half of what this comparison session found — RDP sound dying after a
legacy-path resize — was a bug, not a gap, and is recorded in
[`architecture.md`](architecture.md)'s RDP and audio sections.
