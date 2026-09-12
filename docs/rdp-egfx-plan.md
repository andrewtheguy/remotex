# EGFX (MS-RDPEGFX) for the gateway's own RDP client

## Context

The gateway speaks RDP with its own from-scratch client (`src/rdp_client/`), no
IronRDP. It carries the desktop as plain fast-path **Bitmap Updates** decoded by the
planar codec; the Graphics Pipeline (MS-RDPEGFX) is not advertised at all. Commit
`13e6c8f` on `rdp-refactor` deleted the previous EGFX support because it rode on
IronRDP's pipeline and its RFX Progressive decoder failed mid-session on some hosts.
`main` still carries the `egfx` config key shape (default **on**) which was removed on
this branch.

The goal is to carry EGFX again, this time in the gateway's own protocol code. The
payoff is a cheaper resize (a graphics reset instead of a full
Deactivation-Reactivation Sequence) and desktop content decoded through the modern
pipeline. Everything downstream of the framebuffer (tiles, motion, video, the
pointer, the clipboard) is unchanged — EGFX only changes how the desktop's pixels
arrive.

**Decisions (confirmed with the user):**
- **Modern Windows only.** The target is a current Windows RDS host, matching the rest
  of `rdp_client`. No xrdp / other-server EGFX behavior, no legacy fallbacks inside the
  pipeline. An unexpected codec or PDU is logged, never guessed at.
- **`egfx` defaults on**, exactly as `main` (`egfx()` → `unwrap_or(true)`), with
  `egfx = false` selecting the legacy bitmap path as the escape hatch. Refused on VNC.
- **Staged PRs** off `rdp-refactor` (AGENTS.md: no squash merges, keep up to date with
  main, version bump on the PR branch before merge). Each stage builds, passes
  `cargo clippy --all-targets -- -D warnings` and `cargo test`, and is QA'd against the
  `windows-ent-sandbox` target in `tmp/test_uat.toml` before merge.
- **H.264 (AVC420/AVC444) is out of scope.** We advertise it disabled
  (`RDPGFX_CAPS_FLAG_AVC_DISABLED`) so the host never sends it. No H.264 decoder.

Reference C is under `tmp/references/FreeRDP` (`channels/rdpgfx/client/rdpgfx_main.c`,
`libfreerdp/gdi/gfx.c`, `libfreerdp/codec/{zgfx,progressive,rfx_*,clear,nsc,planar}.c`).
Ported to Rust against `src/rdp_client/proto/wire.rs`'s bounds-checked reader/writer,
in the house style: refuse an unknown field by name rather than skip it.

## How EGFX rides the existing client

- **Transport.** EGFX is a *dynamic* channel named `Microsoft::Windows::RDS::Graphics`
  opened by the server over `drdynvc` — the same static channel Display Control already
  uses (`proto/dvc.rs`, `proto/channel.rs`). `dvc::Incoming` already reassembles a
  server's Data First / Data split, so large PDUs arrive whole. So `drdynvc` must be
  asked for when `egfx` OR `resize` is set (today: `resize` only, in
  `connect::wanted_channels`).
- **Enabler.** The client must set `RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL` (0x0100) in
  `earlyCapabilityFlags` of GCC `CS_CORE` (`proto/gcc.rs`) — verify against MS-RDPBCGR
  2.2.1.3.2 in Stage 1 (if wrong, the host simply never opens the Graphics channel,
  which the probe catches).
- **Framing.** A server-to-client channel PDU is ZGFX-compressed (RDP8 bulk), then
  one or more RDPGFX PDUs (`RDPGFX_HEADER`: cmdId u16, flags u16, pduLength u32).
  Client-to-server PDUs go raw — Stage 1 proved the host reads their header straight
  off the channel and fails its graphics subsystem if they are wrapped.
- **Frames.** EGFX brackets updates with StartFrame / EndFrame carrying a frameId; the
  client MUST reply `RDPGFX_FRAME_ACKNOWLEDGE` per EndFrame (queueDepth
  `QUEUE_DEPTH_UNAVAILABLE` = 0) or the host throttles then stalls. This gives a *real*
  frame boundary, so reintroduce `Event::Frame` (deleted in `13e6c8f`) and the engine's
  frame-marked flush regime.
- **Geometry.** Under EGFX a monitor-layout resize is answered by `RESETGRAPHICS`
  (new size + monitor defs), not a Deactivation-Reactivation Sequence. It surfaces as
  the same `Event::Resize`, so `rdp.rs`'s `applied`/`confirms` layout logic is reused
  unchanged. The legacy reactivation path stays for `egfx = false`.
- **Surfaces.** The host creates surfaces, maps one (or more) to the output at an
  origin, draws into them with wire-to-surface / surface-to-surface / cache-to-surface
  / solid-fill, and commits on EndFrame. Model it like `gdi/gfx.c`: keep each surface's
  own RGBX32 buffer + invalid region, composite each *mapped* surface's invalid region
  into the single `Framebuffer` on EndFrame, and emit those rects as `Event::Paint`.
- **Pointer / clipboard** are unchanged — both still travel their own way.

## Stage 1 — Foundation + measurement probe (PR 1)

Lands the channel, ZGFX, the PDU layer, the surface compositor, and the two simplest
codecs; **measures** which codecs/PDUs the sandbox actually sends so Stages 2–3 are
driven by observation, not guesswork.

New files:
- `src/rdp_client/proto/zgfx.rs` — RDP8 bulk decompression (port of `zgfx.c`): the
  token table, a bit reader, the 2.5 MB history ring (a stateful `Zgfx` kept for the
  channel's life), single + multipart segments, 64 KiB per-segment output cap. Unit
  tests from FreeRDP's own vectors.
- `src/rdp_client/proto/gfx.rs` — the RDPGFX PDU layer: `RDPGFX_HEADER`, a `Message`
  enum decoded from a decompressed buffer (which may hold several PDUs), plus
  `caps_advertise()` and `frame_acknowledge()` encoders, and the codecId / capversion
  constants. Caps advertised: CAPVERSION_8 and _10 with `SMALL_CACHE` and
  `AVC_DISABLED`; `THINCLIENT` deliberately **not** set (Windows > 8.1 ignores it and
  it would push non-progressive RemoteFX). Unhandled PDUs are logged, not fatal.
- `src/rdp_client/gfx.rs` (sibling of `framebuffer.rs`) — surface + cache state and the
  compositor. Stage 1 handles ResetGraphics, Create/DeleteSurface, MapSurfaceToOutput,
  Start/EndFrame, and WireToSurface_1 for **uncompressed** and **planar**; every other
  codecId is tallied and logged (`debug!`) so the region is left as-is but the session
  survives. Returns damage rects for the engine.

Changed files:
- `proto/planar.rs` — extend to EGFX planar as Windows sends it (XRGB/ARGB 8888, the
  alpha plane, CLL/CS only if the probe shows them). Keep the existing Bitmap-Update
  callers working.
- `proto/gcc.rs` — set 0x0100 in `earlyCapabilityFlags` when EGFX is on; thread a
  `gfx: bool` through `ConferenceCreateRequest`.
- `proto/dvc.rs` / `session.rs` — accept the Graphics channel by name in `answer()`
  (today only Display Control is taken), route its Data through
  `zgfx → gfx::Message → compositor`.
- `connect.rs` — `wanted_channels` opens `drdynvc` when `egfx || resize`; thread
  `egfx` into the GCC request.
- `session.rs` — `Connect.egfx`; reintroduce `Event::Frame` (on EndFrame) and emit
  `Event::Resize` from ResetGraphics; send `frame_acknowledge` per EndFrame.
- `framebuffer.rs` — reintroduce a stride-aware `blit` (surface buffer → framebuffer);
  keep the packed-blit test coverage from `13e6c8f`.
- `src/rdp.rs` — reintroduce the `frame_marks` flush regime and `FRAME_NET` (both
  deleted in `13e6c8f`); `connect_config` passes `config.egfx()`.
- Config revert of `13e6c8f` (re-add exactly as `main`): `TargetConfig.egfx:
  Option<bool>`, `egfx()` → `unwrap_or(true)`, the VNC-refusal validation and its test,
  the `egfx: None` fields in the struct literals in `src/server.rs`, `src/session.rs`,
  `src/ws.rs`, `tests/auth_e2e.rs`, the `graphics` line in
  `src/embedded/manager.rs::target_specs`, and the `remotex.example.toml` / `README.md`
  / `docs/architecture.md` / `docs/rdp-client.md` / `docs/roadmap.md` passages.
- `tests/rdp_client_probe.rs` — `Connect { egfx: true, .. }`, re-add the `frames`
  tally, and print the codecId / PDU distribution the host produced.

Stage-1 QA (see Verification): run the probe against `windows-ent-sandbox`, capture
which codecIds and PDUs appear. That capture is the input to Stages 2–3.

### Stage 1 — measured (2026-09-12, `windows-ent-sandbox`, Windows RDS)

Landed and QA'd. The channel opens, the host confirms **CAPVERSION_10** (flags
`0x22` = SMALL_CACHE | AVC_DISABLED), frames flow and are acknowledged, and each
monitor-layout resize is a **RESETGRAPHICS** (1280→1600→1280) with no reactivation.
The session survives a full run; `egfx = false` still lights the desktop over bitmap
updates (≈1.02M of 1.024M pixels), and the clipboard round-trip passes under EGFX.

One caught bug worth recording: **client→server RDPGFX PDUs go out raw, not
ZGFX-wrapped.** Only the server→client direction is bulk-compressed; a Windows host
reads the RDPGFX header straight off the channel for the caps advertise and the
frame acknowledgement, and wrapping them made it read the descriptor byte as a
command id and end the session with `ERRINFO_GRAPHICS_SUBSYSTEM_FAILED` (0x112f).

The codec/command distribution over ~253 frames diverges from the guess above and
**reorders Stages 2–3**:

| what the host sent | count | carried by |
| --- | --- | --- |
| ClearCodec | 569 | WireToSurface_1 |
| RemoteFX Progressive | 27 | WireToSurface_2 (+ DeleteEncodingContext) |
| CacheToSurface | 2184 | — |
| SurfaceToCache | 585 | — |
| SolidFill | 13 | — |
| SurfaceToSurface | 7 | — |

No PLANAR, no UNCOMPRESSED, no NSCodec, no plain RemoteFX (CAVIDEO). So on this
host **ClearCodec is the primary desktop codec and the caches are load-bearing** —
the desktop is dark until both are decoded, which is why Stage 1's probe asserts
frames rather than lit pixels under EGFX. Progressive alone (the plan's Stage 2)
paints only a small share; ClearCodec and CacheToSurface/SurfaceToCache (the plan's
Stage 3) are what a lit desktop needs here. The two later stages are best taken
together, or Stage 3 first, rather than in the written order.

## Stage 2 — RemoteFX Progressive (PR 2)

The decoder that actually paints the desktop. New `src/rdp_client/proto/progressive.rs`
(with the RFX helpers it needs — RLGR1 decode, differential decode, scalar
dequantization, the 3-level inverse DWT, YCbCr→RGB), porting `progressive.c` +
`rfx_rlgr.c` + `rfx_dwt.c` + `rfx_quantization.c` + `rfx_differential.h`. Per-surface
tile cache (64×64), region parsing, simple/first/upgrade tiles, subband-diff and
DWT-extrapolate flags. Wire into `gfx.rs` for CAPROGRESSIVE (and _V2 if the probe shows
it). Unit tests on the transforms with known vectors; QA proves the desktop renders
sharp, resizes via ResetGraphics, and survives a long session (the failure mode the
old decoder had).

## Stage 3 — Caches, copies, ClearCodec (PR 3)

`gfx.rs` gains SurfaceToSurface, SurfaceToCache, CacheToSurface, SolidFill, EvictCache
(scroll and repeat, the common case). New `src/rdp_client/proto/clear.rs` (ClearCodec:
glyph/vBar/short-vBar caches, RLEX and NSCodec subcodecs) wired for CLEARCODEC. Add
`proto/nsc.rs` only if the Stage-1 tally showed the host actually sends NSCodec. QA: a
full desktop with menus and scrolling, no holes, over an extended run.

## Verification (each stage)

- `cargo clippy --all-targets -- -D warnings` and `cargo test` (do not run `cargo fmt`).
- Live QA against the reachable sandbox (TCP 3389 confirmed open):
  ```sh
  REMOTEX_UAT_TARGET=windows-ent-sandbox \
    cargo test --test rdp_client_probe -- --ignored --nocapture --test-threads 1
  ```
  Stage 1 reads its codecId/PDU tally from this. Later stages assert a lit, resized,
  repainted desktop (the probe's existing `lit()` and resize checks) with EGFX on.
- Browser QA (Stages 2–3): `bun run build` in `frontend/`, then `remotex serve` with a
  config pointing at the sandbox; the frontend is unchanged so this only confirms the
  engine. Inspect control messages with `tests/ws_probe.py`. Per AGENTS.md, ask the
  user for the visual confirmation only eyes can give (no screenshot loops).
- Docs updated as each stage lands: `docs/rdp-client.md` Graphics section,
  `docs/architecture.md`, `docs/roadmap.md` (move EGFX out of "planned"), `README.md`.

## Risks

- **Progressive decoder correctness**, ported blind (Stage 2) — the single largest risk
  and the reason it is its own stage behind a working, measured foundation.
- **The 0x0100 early-capability flag / caps shape** — if wrong the Graphics channel
  never opens; Stage 1's probe catches it immediately.
- **ZGFX history ring** off-by-one corrupts every later PDU — covered by unit vectors.
- **`egfx` defaults on**, so this reaches every RDP target on merge; the legacy path
  stays one config key away and each stage is QA'd before merge.
