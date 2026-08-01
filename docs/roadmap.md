# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Render dial — the modes still planned

The gateway used to encode every tile as lossless PNG. It now has a two-axis
per-target render dial — a `render_type` (quality strategy) and a
`render_subtype` (codec), plus `render_quality` — whose lossy combinations
`fixed-quality` + `jpeg` and `fixed-quality` + `webp` (every tile encoded at a
fixed quality; WebP is ~30% smaller than JPEG) are **implemented**. The default,
`full` + `png`, is byte-identical to the PNG-only gateway. Zero wire change: the
tile format byte carries the codec and both clients decode all three. Detailed
proposal and the full type×subtype matrix:
[proposals/quality-dial.md](proposals/quality-dial.md).

What remains planned are further points on those two axes, each a new enum variant
the config already refuses by name until it is built:

- **`adaptive-jpeg` subtype** — a per-tile PNG/JPEG classifier (flat UI and text
  stay lossless, photographic tiles go JPEG), so a fixed quality no longer softens
  text. The content-based cousin of the motion scheme below.
- **`adaptive` type** — quality chosen automatically rather than fixed: from how
  fast a region is changing, or from the connection's speed.
- **`video` subtype** — an inter-frame codec for full-motion regions.

The dynamic, motion-adaptive form of `adaptive` — quality chosen per cell from how
fast it is changing, with a cleanup pass when it settles — is the scheme the
deleted rxa agent ran. It is deferred until the fixed dial proves insufficient; the
design and its salvage point are recorded in
[proposals/motion-adaptive-jpeg.md](proposals/motion-adaptive-jpeg.md).

### Apple Screen Sharing display modes

**Done for the supported shapes.** `subtype = "ard"` is Standard Screen Sharing: it
lists the Mac's physical screens with an *All Displays* entry, binds one with
`SetDisplayMessage`, and reports each screen's pixel density. The checkmark follows
the Mac's answering layout rather than the click.

`subtype = "ard-high-performance"` is High Performance Screen Sharing and requests
one virtual display at the target's configured `width` and `height`. It uses Apple's
003.889 record transport and adds zlib. Apple's own client exposes undocumented
one/two-virtual-display, resolution-preset, and dynamic-resolution controls in this
mode; they are intentionally not roadmap items here. The gateway sends one display
configuration during setup and never changes it from a viewport report.

The 003.889 wire constraints remain load-bearing: the first `SetEncodings` is the
measured exact list, zlib is asked for only after the first layout, and a layout
payload is two bytes shorter than its own length prefix says. The technical details
remain in [`apple-vnc-889.md`](apple-vnc-889.md).

Two display-density items remain planned:

- **Make All Displays point-correct on mixed-density Macs.** Apple's combined
  framebuffer is a mosaic of each screen's backing pixels, but the current wire
  `Resize` describes the whole canvas with one scale. No one scale is true for a
  1× display beside a 2× display, so the combined view falls back to 1× and the
  Retina screen appears at twice its logical size. Apple's own client instead
  composes each screen in logical coordinates. The gateway needs a density-aware
  compositor that normalizes each screen's backing rectangle into one logical
  coordinate space, with the corresponding tile and pointer transforms.
- **Avoid sending unused Retina pixels for one selected display.** A selected 2×
  display is point-correct today because clients retain its full-resolution
  framebuffer and present it at half the CSS or AppKit size. On a 1× viewer that
  means transmitting and decoding four source pixels for every physical output
  pixel. A future gateway-side density conversion should use the client's reported
  host scale to resize the framebuffer and tiles before they enter the wire, while
  preserving the remote's logical size and input coordinates. This is a bandwidth
  optimization, not fit-to-window scaling.

What remains on that wire, each for its own reason:

- **The high-performance pasteboard.** Apple carries it over messages of its own —
  an announcement, a fetch, then a zlib'd multi-flavour archive. Standard `ard`
  implements that protocol on its plain stream; `clipboard` remains refused on
  003.889 until the same messages are enabled on its record transport.
- **Apple's still-image codecs** (`0x3ea` and the per-tile `0x3f3`), which would
  compress far better than zlib. Blocked, not deferred: the reference this was
  written from marks `0x3ea`'s rectangle body and `0x3f3`'s command-code table as
  unresolved, so they cannot be written from it — and a client must not advertise an
  encoding it cannot decode. ZRLE is the decodable middle ground if zlib ever proves
  insufficient.
- **The Adaptive media path**: HEVC and AAC over SRTP/UDP, negotiated by
  `MediaStreamOptions`. A large amount of work for a link that is usually a LAN, and
  the negotiation's own schema is only partly recovered.
- **Authentication types 33, 35 and 36** (RSA-SRP, Kerberos, direct SRP). Type 30's
  Diffie-Hellman reaches the same record layer and is what the gateway already had;
  the others are only needed against a Mac that stops offering it.
- **A second rekey.** macOS sends one per session; a mid-session one closes the
  session with a named error rather than being installed, because swapping the
  ciphers on both halves of a running session at the same instant is not something
  the two-task shape supports.

## Not planned

### Multiple sessions

**Concurrent sessions, shared sessions, and a session broker are outside the
product model.** This is one user's program, and that is not a limitation waiting
to be lifted.

There is one active session slot: one active session per gateway instance,
permanently. A new browser takes over and evicts the previous holder
(`src/session.rs`), which a client offers with a Take over button — the same
shape as Windows Remote Desktop. A reconnect, a target switch and a browser
takeover all reclaim the slot in silence: they are the same session coming back,
whatever else has changed.
