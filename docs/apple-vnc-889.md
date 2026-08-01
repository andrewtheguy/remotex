# Apple RFB 003.889, as measured

Corrections and confirmations for the reverse-engineered specification this
gateway's `ard-high-performance` subtype was written from, gathered by speaking the
protocol by hand against **macOS 26.5.2** (an Apple Virtualization guest) in
July 2026.

Read this *with* that document, not instead of it: the framing, offsets and
key derivations there are mostly right, and what follows is only the places where a
live Mac disagrees — plus the ones worth knowing are confirmed, because a
reverse-engineered document gives no way to tell a measured field from an inferred
one.

The implementation is `src/vnc_record.rs` (the 003.889 transport),
`src/vnc_apple.rs` (messages shared by both Apple subtypes), and the two Apple
paths in `src/vnc.rs`.

> **This document was substantially wrong in its first revision**, which claimed a
> 003.889 session cannot enumerate or pick displays and cannot learn a screen's
> pixel density. It can do both. The mistake was that this gateway was *asking* the
> Mac to hide its screens, and did not know it. What follows is the corrected
> account; §"The display finding" is the part that changed.

## Summary

| | |
|---|---|
| Confirmed | The handshake, type-30 authentication and its wrap key; the rekey; the record layer in full; zlib; the cursor cache; the metadata encodings. |
| **Wrong** | `AutoFrameBufferUpdate` does not make the server stream. `SetDisplayConfiguration` must **not** be sent — it is what hides the Mac's real screens. A display record's fields are two bytes later than documented. A layout payload is two bytes **shorter** than its own length prefix says. `ViewerInfo`'s body carries no strings. |
| **Found** | Displays *can* be enumerated and picked, and each one states its own pixel density. Also: ServerInit's name field is structured, the metadata encodings arrive as bare messages as well as rectangles, and the first `SetEncodings` decides whether any of it happens. |

## The display finding, which is the important one

**`SetDisplayConfiguration` (`0x1d`) is what makes a Mac synthesize a display and
hide its real ones.** Omit it and the Mac shares its actual screens, names them, and
states each one's scale factor.

Enumerated with `CGGetActiveDisplayList` over SSH on a Mac with two distinct,
non-mirrored displays (1280×800 at (0,0) and 1600×900 at (1280,0)):

| session | `CGGetActiveDisplayList` says | `AppleDisplayLayout` says |
|---|---|---|
| none | `active: 2` — id 1 at 1280×800, id 4 at 1600×900 | — |
| plain RFB 3.8 (`subtype = "ard"`) | `active: 2` — unchanged | **both real screens, ids 1 and 4, densities 1 and 2** |
| 003.889 **with** a `0x1d` descriptor | `active: 1` — one display at 4480×1800 | one screen, fresh id (10, 11, 12, 15, 16 — it increments every session), density 1 |
| 003.889 **without** `0x1d` | `active: 2` — unchanged | **both real screens, ids 1 and 4, densities 1 and 2** |

The bare *static* descriptor is enough to trigger it — the one the reference
presents as asking for the Mac's real screens at their current size, with no
dynamic-resolution flag and `display_type = 0`. There is no descriptor that means
"leave my screens alone"; not sending the message is what means that. The
synthesized display is sized to the union of the real ones, is not a mirror
(`CGDisplayIsInMirrorSet` is false), and is destroyed on disconnect.

The first revision of this document tested "0x1d omitted" and recorded that the
virtual display appeared anyway. That measurement was confounded: those runs were
made against a Mac that already had another 003.889 session open from the gateway
under test, and a second concurrent session does not see the same display state.
**Kill every session before measuring anything here.**

The Standard-mode result was remeasured separately on July 31, 2026: after Apple
DH authentication, replying `RFB 003.008` and sending the same ten-entry metadata
encoding list produced `AppleDisplayLayout` unsolicited. Selecting ids 4 and 1
then produced 3200×1800 at 2x and 1280×800 at 1x respectively. Standard mode
therefore uses the same display protocol without the 003.889 record layer; remotex
keeps its pixels raw as the uncompressed alternative.

Its native pasteboard also works on that stream, but only after the Standard
client prelude: `ViewerInfo`, `SetMode(control)`, then `AutoPasteboard(start)`.
Without the first two messages the Mac accepts browser-to-Mac writes and explicit
fetches but does not emit the `MiscStatus(cmd=2)` notification needed for automatic
Mac-to-client synchronization.

### Picking a screen

`SetDisplayMessage` (`0x0d`) works in both directions, and the Mac confirms by
echoing its choice in the next layout's `current_display`:

| sent | `current_display` comes back | framebuffer becomes |
|---|---|---|
| `combine_all_displays = 1` | `0xffffffff` | 4480×1800 — the union |
| `display_id = 4` | `4` | 3200×1800 — that screen's own pixels |
| `display_id = 1` | `1` | 1280×800 |

So a client never has to assume a selection took: the layout is the answer, and
`src/vnc.rs` moves the checkmark only when one arrives.

### Per-display density and the combined compositor

Each display record carries **its own scale factor** as a big-endian `f64`: 1.0 for
the 1280×800 screen, 2.0 for the Retina one. It agrees exactly with the ratio of
that record's two bounds rects (3200/1600), so the two can be cross-checked.

The consequence is that **a combined framebuffer has no single density.** 4480×1800
is a 1× 1280×800 beside a 2× 3200×1800, spanning 2880×900 points; no one scale
describes it, and the header's own backing-pixels-to-logical-points ratio
(4480/2880 = 1.56) is a meaningless number that happens to look plausible — it is
neither screen's density and not an average of anything. The gateway therefore
retains that source framebuffer, downsamples damage within each screen's backing
bounds into its logical bounds, and sends clients a 2880×900 framebuffer at 1×.
Pointer coordinates take the inverse mapping. A selected screen bypasses the
compositor and carries its own exact density instead.

## The other corrections

### The first `SetEncodings` decides whether displays are reported at all

This is the strangest thing measured here and it is unexplained. The list in
`vnc_apple::ENCODINGS` — Raw, `0x44c`, `0x44d`, `0x44f`, `0x450`, `0x451`, `0x453`,
`0x455`, `DesktopSize`, `LastRect`, in that order — produces an
`AppleDisplayLayout` on every connection. **Every** variant tried produces none at
all:

| variant | layouts in 7 s |
|---|---|
| the list above | 1–2, every run (6 runs) |
| plus zlib (first or last) | 0 |
| plus `DeviceInfo` (`0x456`) | 0 |
| plus `UserInfo`, `CursorPos`, `DisplayInfo`, `DesktopSize` or `LastRect` (already present, appended again) | 0 |
| minus any single entry (5 tried, one of them 5 times) | 0 |
| the same set, reversed | 0 |
| the same set, with Raw duplicated | 0 |

Reversal failing while the set is unchanged rules out set membership; a duplicate
failing rules out a simple capability test. Sixteen variants, one fresh connection
each, no exceptions in either direction. What `screensharingd` is doing with this
list is a **revision gap**; that it does something is not.

Two consequences for an implementer:

- **zlib cannot be in the first `SetEncodings`.** Send a second one, with zlib
  appended, once a layout has arrived: the Mac keeps its display state and simply
  changes encoder. Measured at 398 KB for a 3200×1800 frame against 23 MB raw, with
  display selection still working afterwards. This is what `ENCODINGS_WITH_ZLIB` and
  the `asked_for_zlib` flag in `src/vnc.rs` are for.
- **Advertising is a promise.** Every entry in that list has to be decodable or at
  least steppable, and two of them do not share the common length rule: `CursorPos`
  (`0x44c`) has no payload at all, and `DisplayInfo` (`0x44d`) is four `u16`s then
  `0x1c` bytes per screen. `LastRect` has to actually end the update, which means
  handling the `0xffff` rectangle count that goes with it. (`UserInfo` (`0x44e`) — a
  counted name then a counted image — is *not* advertised but is decoded anyway, on
  the same grounds as `DeviceInfo`: tolerating an encoding that turns up unasked
  costs nothing, and desyncing on it costs the session.)

**One honest qualification to all of the above.** The list the probe measured had
eleven entries; `vnc_apple::ENCODINGS` ships ten, `UserInfo` having been dropped
between the two. That single-entry removal was *not* among the sixteen variants tried,
and the shipped ten-entry list was afterwards verified against the same Mac to produce
a layout, a display list and a working selection — twice, through the gateway rather
than the probe. So "any single removal costs the layout" is what the bisection
measured, and it has one known exception. Which entries actually matter is therefore
still unresolved, and the safe reading is the one in the code: leave the list alone.

### A layout payload is two bytes shorter than it declares

The `u16` prefix counts the whole payload including itself — `0x14 + displays ×
0x38`, which is 132 for two screens and 76 for one — but **two fewer bytes are
sent**. The final display record stops after its last field and omits its two
trailing pad bytes.

Consuming the declared count therefore eats the first two bytes of the message
behind it, and the failure is about as far from the cause as it could be: the next
read is a framebuffer update whose rectangle count is really a screen width
(`0x0c80` = 3200), and the session dies several messages later complaining about an
encoding nobody sent (`0xdaffdada`, which is pixels). Both the probe and the gateway
hit it, which is how it was found — it appears on the *second* layout of a session,
so it needs a display switch to reproduce.

### A display record's fields are two bytes later than documented

The tell is the `f64` `3ff0000000000000` (1.0), which §8.4 places at `+0x00` and
`+0x08` and a live Mac places at `+0x02` and `+0x0a`. Reading the documented offsets
yields `display_id = 0` for every screen and denormal garbage for the scales.

Both bounds rects are **`(top, left, bottom, right)`**, not the `(x, y, w, h)` the
document models; a size is a difference of edges. The measured record:

```text
+0x00 u16      unidentified (0x0002 on the main screen, 0x0000 on the other)
+0x02 f64 BE   this screen's scale factor    -- 1.0 or 2.0
+0x0a f64 BE   viewer scale factor           -- always 1.0
+0x12 u32 BE   display_id (CGDirectDisplayID)
+0x16 rect     logical bounds  (u16 top, left, bottom, right)
+0x1e rect     backing bounds  (u16 top, left, bottom, right)
+0x26 u32 BE   flags: bit0 = main, bit1 = in mirror set
+0x2a          pixel format (bpp, depth, big-endian, true-colour, maxes, shifts)
```

And the header, which is 0x14 bytes including the length prefix:

```text
+0x00 u16  payload length, two more than is sent
+0x02 u16  version = 5
+0x04 u16  logical width  -- the whole desktop, in points; does not change on a selection
+0x06 u16  logical height
+0x08 u16  backing width   -- THE FRAMEBUFFER, and what does change on a selection
+0x0a u16  backing height
+0x0c u32  current_display, 0xffffffff for the combined view
+0x10 u32  unidentified; read 4 on every layout of every session, selected or not
```

Ground truth these offsets reproduce, measured separately over SSH: ids 1 and 4,
1280×800 at (0,0) and 1600×900 at (1280,0), the first one main, the second Retina.
`src/vnc_apple.rs` pins a captured payload byte for byte against exactly that.

### ServerInit's name field is not a name

It is 22 bytes of structure and then the name, which noVNC-ARD
(`tmp/programs_for_reference/noVNC-ARD/ard/ard-patch.js:300-327`) decodes as:

```text
+0x00  u8    zero marker (only byte 0 is checked)
+0x01  u8    unread
+0x02  u32   server flags
+0x06  16B   capability bitmap
+0x16  ...   the UTF-8 name
```

Flags: `0x01` observe, `0x02` may-control, `0x04` **session-select**, `0x08`
no-virtual-display. The test VM reads `0x00000052` — may-control, plus unidentified
`0x10` and `0x40` — and its name comes out as `"Andrew's Virtual Machine"`, which
printing the whole field as latin-1 turned into mojibake.

`0x04` is worth reading even though nothing acts on it: a server that sets it
expects a `SessionInfo` → `SessionCommand` → `SessionResult` exchange *before*
anything else, where command 1 is "connect to the console" and command 2 is "connect
to a virtual display". No Mac measured here offers it, so it is unimplemented — and
`describe_desktop` in `src/vnc.rs` says so in the log rather than leaving a session
that stops in silence.

### `AutoFrameBufferUpdate` (`0x09`) does not make the server stream — §8.11, R-A16b

The document says it "switches the server to server-driven framebuffer streaming"
and that "after sending this, a client SHOULD NOT continue to poll".

macOS 26 does not stream. Armed or not, it answers a non-incremental
`FramebufferUpdateRequest` and is otherwise silent — including while the screen is
visibly changing. Measured by sending pointer events on a 2.5-second cycle and
never re-requesting: **zero rectangles in 25 seconds.** The same cycle with a
non-incremental request appended returns a full update every time.

A client that follows the document paints one frame and then freezes. **Keep
polling.** Sending `0x09` anyway is harmless and is what the document says keeps
cursor updates alive across a login or lock transition, which is why this
implementation still sends it — but it is not the update driver.

### `ViewerInfo` (`0x21`) has no strings in it — §5.5

The document frames the body as `u16 version`, `u16 viewer_app`, *version strings*,
`byte[32] capability_bitmap`, without ever saying how the strings are framed. There
is no such thing: they are two numeric triples. 66 bytes total, `body_len = 62`:

```text
u8 0x21 | u8 pad | u16 body_len = 62
u16 appClass = 1 | u32 appId = 2
u32 app version  = 6, 1, 0        (three u32be)
u32 os version   = 15, 0, 0       (three u32be)
byte[32] capability bitmap        ([0]=0xb0 [2]=0x0c [3]=0x03 [4]=0x90 [10]=0x40)
```

2 + 4 + 12 + 12 + 32 = 62 exactly, and that bitmap decodes MSB-first to the
`{0, 2, 3, 20, 30, 31, 32, 35, 81}` the document observed — so its bitmap was right
and only the framing was wrong.

The first revision of this document recorded "ViewerInfo must not be sent", because
a body built from the string description is mis-sized: macOS reads more bytes for
the message than its own `body_len` declares, swallows the `SetEncryption` behind
it, and waits forever with no error from either end. Sending the 66 bytes above
does not do that. **It is also not required** — the layout arrives with or without
it — so this gateway does not send one, and the shape is recorded here rather than
in code that nothing calls.

### The metadata encodings also arrive as bare messages

`0x451` also comes as message type `0x51`, `0x453` as `0x53`, and so on: the message
type is the encoding's low byte, with the same `u16`-length framing. A live session
sends both forms of the same content. There are also two zero-payload message types,
`0x04` (ServerAck) and `0x07` (NOP).

A client that does not tolerate these ends the session on the first one, which for
this gateway used to be "unknown server message type 4" a few seconds in.

## Confirmed

Worth stating, because a reverse-engineered document offers no way to tell a
measured claim from an inferred one, and these carried the most risk.

**The record layer, in full and in both directions.** AES-128-CBC with one
persistent context per direction, never reset — record N's last ciphertext block is
record N+1's IV. `u16 ciphertext_len` outside, `u16 body_len || body || filler ||
byte[20] integrity` inside, `filler_len = (-(2 + body_len + 20)) mod 16`, and
`integrity = SHA1(u32_be(seq) || plaintext[0 .. len-20])` with independent
non-resetting per-direction sequence counters from 0. Every record of every session
verified its trailer, and the Mac accepted everything sent back the same way. Zero
filler is accepted (the document permits zero or random).

**Reassembly by concatenation is mandatory, not an edge case.** A full-screen zlib
rectangle is ~400 KB against a 65 520-byte record ceiling, so it spans several
records on the first update after compression is negotiated. A parser that treats
one record as one message desyncs immediately — the first thing that happened to the
probe used here.

**Type-30 authentication and its wrap key.** `MD5(shared)` is the AES-128 key for
the credential blob *and* the record layer's first wrap key, exactly as documented.

Note that §4.2.3 says the credential blob is AES-128-**CBC** with a zero IV, and
§13.1 repeats it. **It is ECB** — each block independently — which is what this
gateway has always sent and what macOS accepts. The document flags its own type-30
section as having no capture behind it.

**The rekey.** Delivered as a single-rectangle FramebufferUpdate with `x=y=w=h=0`
and encoding `0x44f`; body `u32 generation || 16B wrapped key || 16B wrapped iv`,
each half AES-128-ECB-decrypted independently under the wrap key. `generation` is 1.
Only ever one per session, so multi-rekey remains unexercised.

**zlib (`0x06`).** `u32 length` then a chunk of **one deflate stream for the life of
the connection**, inflating to exactly `w × h × 4`. Confirmed with an independent
inflater. Roughly 50:1 on a static desktop, which is the reason to use this subtype
at all — see the note above about which `SetEncodings` it may appear in.

**The cursor cache (`0x450`).** STORE when `compressed_len > 0`, SELECT when zero,
and the payload really is a `w·h·4` BGRA pixmap followed by a **separate** `w·h`
alpha plane — folding the fourth pixel byte in as alpha produces a uniformly opaque
cursor. Each STORE starts an **independent zlib stream**; it does not share the
connection-wide inflater used by framebuffer encoding `0x06`, nor the inflater from
the preceding cursor. A malformed STORE can therefore be consumed and ignored
without poisoning the next cursor or ending the desktop session. Real cursors
arrived and rendered.

**The metadata encodings** `0x453`, `0x455`, `0x456`. All three frame themselves the
same way — a `u16` giving how much follows — so one rule steps over all of them
without desyncing.

## Still unknown

- **What `screensharingd` does with the first `SetEncodings`**, such that adding,
  removing or reordering one entry costs the whole display layout. The open
  question, and the one place this implementation depends on a constant nobody
  understands.
- The word at `+0x10` of a layout header (reads 4, always) and the `u16` at `+0x00`
  of each display record (2 on the main screen, 0 elsewhere).
- Whether the session-select exchange works, no Mac here having offered it.
- Apple's still-image codecs `0x3ea` and `0x3f3`; the document leaves the first's
  rectangle body and the second's command-code table unresolved, and neither was
  advertised here, so nothing was learned.
- The Adaptive media path (`0x1c`, SRTP, HEVC): not attempted.
- Authentication types 33, 35 and 36: not attempted, type 30 being sufficient.
- Multi-rekey, and whether sequence counters survive a second one.
- Dynamic resolution, which is what a full dynamic `0x1d` descriptor drives — and
  which is now doubly interesting, since `0x1d` is the message that hides the real
  screens. Asking for a resizable virtual display and asking to see the Mac's own
  screens may simply be exclusive.

## Reproducing any of this

The probe was throwaway Python speaking the protocol by hand — deliberately not
calling into `src/`, so a misreading on one side could not be agreed with by the
other. It lived at `tmp/apple889_probe.py` (gitignored). The shape is: TCP to port
5900, `RFB 003.889\n` both ways, security type 30, the DH exchange above, ClientInit
`0xC1`, ServerInit, `SetEncodings`, `SetPixelFormat`, `SetEncryption(1)` and `(2)`,
read the rekey, then a record layer as specified above around ordinary RFB.

Three instruments did the work, and two of them were outside the protocol:

1. **Enumerating displays over SSH with `CGGetActiveDisplayList` while a session was
   live.** This is what found the synthesized display, and no amount of protocol
   reading would have.
2. **Bisecting one message or one encoding at a time, on a fresh connection each
   time.** This is what found `0x1d`, and what mapped the `SetEncodings` behaviour.
   It only works with **every other session closed** — the gateway included. A
   stale session invalidated a whole round of measurements and is what put the wrong
   conclusion in the first revision of this document.
3. **A rolling log of every byte handed upward, dumped on the first parse failure.**
   Framing bugs here surface many messages after their cause; nothing else would
   have found the two-byte layout length.
