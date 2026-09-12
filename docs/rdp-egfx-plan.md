# EGFX (MS-RDPEGFX) for the gateway's own RDP client

The gateway's from-scratch RDP client (`src/rdp_client/`) carries the Graphics
Pipeline in its own protocol code, staged in three PRs off `rdp-refactor` and each
measured against a live Windows host before the next was written. How the pipeline
works is documented in [The RDP client](rdp-client.md); this page keeps the decisions
that shaped it and the measurements each decoder was picked from.

## Decisions

- **Modern Windows only.** The target is a current Windows RDS host, matching the rest
  of `rdp_client`. No xrdp or other-server EGFX behavior, no legacy fallbacks inside
  the pipeline. An unexpected codec, subcodec or PDU is refused by name in the log,
  never guessed at, so anything a host sends that this client lacks shows up as a
  named refusal rather than a wrong picture.
- **`egfx` defaults on**, with `egfx = false` selecting the bitmap-update path as the
  escape hatch. Refused on VNC.
- **H.264 (AVC420/AVC444) is out of scope.** It is advertised disabled
  (`RDPGFX_CAPS_FLAG_AVC_DISABLED`) so the host never sends it. No H.264 decoder.
- **Only what the host was seen to send.** RemoteFX Progressive implements the
  reduce-extrapolate wavelet and RLGR1 alone; the classic wavelet, RLGR3 and
  Progressive V2 are refused by name rather than carried unexercised. The standalone
  NSCodec bitmap codec is likewise absent: NSCodec exists here only as ClearCodec's
  subcodec, which is the only way the host uses it.

Reference C is FreeRDP, under `tmp/references/FreeRDP` (gitignored):
`channels/rdpgfx/client/rdpgfx_main.c`, `libfreerdp/gdi/gfx.c`, and
`libfreerdp/codec/{zgfx,progressive,rfx_*,clear,nsc,planar}.c`. Ported to Rust against
`proto/wire.rs`'s bounds-checked reader, in the house style.

## Measured against `windows-ent-sandbox` (2026-09-12, Windows RDS)

The live target is the `windows-ent-sandbox` entry in the gitignored
`tmp/test_uat.toml`. The host confirms **CAPVERSION_10** (flags `0x22` = SMALL_CACHE |
AVC_DISABLED), frames flow and are acknowledged, and each monitor-layout resize is a
**RESETGRAPHICS** (1280→1600→1280) with no reactivation.

### What the host draws with

Over ~253 frames of the first measured run, with the foundation alone in:

| what the host sent | count | carried by |
| --- | --- | --- |
| ClearCodec | 569 | WireToSurface_1 |
| RemoteFX Progressive | 27 | WireToSurface_2 (+ DeleteEncodingContext) |
| CacheToSurface | 2184 | — |
| SurfaceToCache | 585 | — |
| SolidFill | 13 | — |
| SurfaceToSurface | 7 | — |

No PLANAR, no UNCOMPRESSED, no standalone NSCodec, no plain RemoteFX (CAVIDEO). On
this host **ClearCodec is the primary desktop codec and the caches are
load-bearing**: the desktop is dark until both are decoded, and Progressive alone
paints only a small share. The caches, copies and ClearCodec were therefore built
before Progressive.

That tally counted codecs, not the subcodecs inside ClearCodec. The first run with
ClearCodec's raw and RLEX subcodecs, the caches and Progressive in lit ≈84% of the
desktop and left 56 ClearCodec rectangles unpainted, every one refusing **subcodec 1,
NSCodec**: this host draws its pictures and anti-aliased text through that one.
`proto/nsc.rs` closed it.

### Where it stands

**≈99.5% of the desktop lit at open and after each resize, zero unpainted rectangles,
zero refusals** over 356 frames (821 ClearCodec, 35 Progressive, 2100 CacheToSurface,
656 SurfaceToCache, 22 SurfaceToSurface, 15 SolidFill). The framebuffer, dumped by the
probe as PNG, shows the desktop as Edge draws it: photographs sharp with correct colour
through Progressive, text crisp through ClearCodec. Every Progressive region the host
sent carried the reduce-extrapolate flag, and every tile kind — simple, first,
upgrade — appeared.

Two host behaviors the specification alone would not have predicted, both handled:

- **Client→server RDPGFX PDUs go out raw, not ZGFX-wrapped.** The host reads the
  caps advertise and each frame acknowledgement straight off the channel; wrapping
  them ends the session with `ERRINFO_GRAPHICS_SUBSYSTEM_FAILED` (0x112f).
- **A `drdynvc` Data First may carry the whole announced payload.** FreeRDP delivers
  it at once, and so does this client.

## Verification

- `cargo clippy --all-targets -- -D warnings` and `cargo test` (do not run `cargo fmt`).
- Live QA against the sandbox, which asserts a lit, resized, repainted desktop and a
  clipboard round trip with EGFX on, and prints the host's codec and command tally:
  ```sh
  REMOTEX_UAT_TARGET=windows-ent-sandbox REMOTEX_UAT_DUMP=tmp/qa/<dir> \
    cargo test --test rdp_client_probe -- --ignored --nocapture --test-threads 1
  ```
  `REMOTEX_UAT_DUMP` writes the framebuffer as PNG at open and after each resize, so
  the decoded desktop can be inspected without a browser. Trust the probe's
  `unpainted` count and the PNG over the lit-pixel percentage.
