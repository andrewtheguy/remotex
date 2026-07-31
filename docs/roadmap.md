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

### Apple Screen Sharing display picking and high performance

macOS Screen Sharing can natively pick a single display: the stock Screen Sharing
app shows a Both Displays / Display 1 / Display 2 choice. The gateway does not do
this yet. Today it shares the Mac's real screen(s) as-is over standard screen
sharing; teaching the VNC/ARD path to enumerate the Mac's displays and bind to one
is not implemented. The `ClientMsg::SelectDisplay` / `ServerMsg::Displays` wire is
kept as scaffolding for exactly that — display picking builds on it rather than
adding new wire — and `src/vnc.rs` currently returns an empty display list.

"High-performance" screen sharing goes one step further: it spins up a resizable
virtual display and allows dynamic resize the way RDP does. That is where `resize`
on an `ard` target becomes real — it is rejected at configuration time today.

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
