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

### Apple Screen Sharing: picking a display

**Done.** Both `subtype = "ard"` and `subtype = "ard-high-performance"` list the
Mac's screens with an *All Displays* entry, bind one with `SetDisplayMessage`, and
report each screen's pixel density — so a Retina Mac finally draws at 100% instead
of twice its size. The Standard subtype carries those Apple extensions on RFB 3.8
and keeps pixels raw; high performance carries them over the record layer and adds
zlib. The checkmark follows the Mac's answering layout rather than the click.

The earlier conclusion here — that a 003.889 session replaces the Mac's screens with
one synthesized display and there is nothing to pick — was wrong, and wrong in an
instructive way: **this gateway was asking for that synthesized display and did not
know it.** Sending `SetDisplayConfiguration` (`0x1d`) at all, the bare static
descriptor included, is what causes it. Not sending the message is what gets the real
screens. The measurement that said otherwise had a second session open against the
same Mac, which does not see the same display state.

Two further constraints on that wire are load-bearing and neither is guessable from
the reference: the *first* `SetEncodings` must be an exact list — adding, removing or
reordering one entry costs the display layout entirely, so zlib is asked for in a
second one after a layout has arrived — and a layout payload is two bytes shorter
than its own length prefix says. Both are measured in
[`apple-vnc-889.md`](apple-vnc-889.md), along with what is still unexplained.

What remains on that wire, each for its own reason:

- **Dynamic resolution**, which is the Apple name for what `resize` means on a VNC
  or RDP target. It needs the *full dynamic descriptor* on `SetDisplayConfiguration`
  — the dynamic-resolution flag set, `display_type = 4`, a populated mode table —
  plus the renegotiation that follows every size change. That is now known to be in
  direct tension with picking a screen: sending **any** `0x1d` descriptor is what
  makes the Mac synthesize a virtual display and hide its real ones, so a resizable
  session and a session showing the Mac's own screens may simply be exclusive. The
  gateway sends no descriptor and `resize` is refused for the subtype at
  configuration time. Note that none of this is ever a reason to scale on the client:
  see the 100% rule in `CLAUDE.md`.
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

### A virtual-display-only macOS utility (deferred, low priority)

BetterDisplay already covers the need, so this is revisited only if more control is
required. A small app that creates a `CGVirtualDisplay` at a chosen size — the mold
BetterDisplay is cut from — would let macOS Screen Sharing share that display over
plain ARD with no bespoke code on either side. The mechanism is salvageable from git
history at commit `8990971` (`crates/rxa-agent/src/virtualdisplay.rs` and the
`virtual_display*` config fields).

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
