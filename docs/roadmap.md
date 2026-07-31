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

**Not solved, and not solvable the way it was attempted.** `subtype =
"ard-high-performance"` ships Apple's RFB 003.889 wire — the record layer and zlib
rectangles — but the reason that wire was reached for was to pick one of a Mac's
screens, and it does not offer that.

Measured on macOS 26.5.2: a 003.889 session makes macOS **synthesize a single
display and remove the Mac's real ones** for the session's duration, with a fresh
`CGDirectDisplayID` each time. It does this whether or not the client sends
`SetDisplayConfiguration`, for `display_type` 0, 2 and 4, with the
dynamic-resolution flag set or clear, and for ClientInit `0x01` as well as `0xC1`.
So no `AppleDisplayLayout` ever arrives and `SetDisplayMessage` is ignored — there
is only ever one display to pick. A plain RFB 3.8 session leaves the real displays
alone, which is the check that rules out coincidence.

**The workaround is `subtype = "ard"`**, which shares every real screen in one
framebuffer. Zooming in on one of them is not available from this gateway.

How Screen Sharing.app populates its Both Displays / Display 1 / Display 2 menu is
unresolved, and the reverse-engineered reference is no help because this is exactly
where it is wrong. Settling it needs a packet capture of that app against a
two-display Mac. The parser, the selection path and the `ServerMsg::Displays` wire
are all implemented and unit-tested, so if that capture reveals a mechanism, the
work is in reaching it and not in handling it. Full measurements, including the two
places the reference is outright wrong, are in
[`apple-vnc-889.md`](apple-vnc-889.md).

What else remains on that wire, each for its own reason:

- **Dynamic resolution**, which is the Apple name for what `resize` means on a VNC
  or RDP target. It needs the *full dynamic descriptor* on `SetDisplayConfiguration`
  — the dynamic-resolution flag set, `display_type = 4`, a populated mode table —
  which makes the Mac create a resizable virtual display, plus the renegotiation
  that follows every size change. The gateway sends the static descriptor instead
  and `resize` is refused for the subtype at configuration time. Note that this is
  never a reason to scale on the client: see the 100% rule in `CLAUDE.md`.
- **Both Displays**, the aggregate — moot while the above holds, since a session
  already has exactly one display and it is not one of the Mac's.
- **The pasteboard.** Apple carries it over messages of its own — an announcement,
  a fetch, then a zlib'd multi-flavour archive — not over RFB's Extended Clipboard,
  which is all `src/vnc_clipboard.rs` implements. `clipboard` is refused for the
  subtype rather than accepted and left inert.
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
