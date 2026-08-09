# Apple RFB 003.889, as measured

Measured corrections and confirmations for the reverse-engineered Apple RFB
specification, captured against macOS 26.5.2 and 26.6 Apple Virtualization guests
in July and August 2026. Read this alongside the source specification: it records
live-Mac disagreements and confirms its highest-risk inferred fields.

The implementation is `src/vnc_record.rs` (the 003.889 transport),
`src/vnc_apple.rs` (Apple messages and encodings), and the two Apple paths in
`src/vnc.rs`.

Display observations below are separated by Standard and High Performance mode;
no conclusions combine state from the two session types.

## Summary

`subtype = "ard-high-performance"` is **experimental**. Everything below is
measurement, not specification: Apple documents none of RFB 003.889, and the
confirmations here hold for the Macs and the macOS version named above rather
than for the protocol. A macOS update is free to invalidate any row. The
dynamic-resolution descriptor has been exercised against the arbitrary-size
boundary and a burst of viewport reports, but remains reverse engineered. Prefer
`subtype = "ard"`, which rides the standard RFB 3.8 wire, where a virtual display
is not required.

| | |
|---|---|
| Confirmed | `subtype = "ard"` is Apple Screen Sharing Standard mode over RFB 3.8 and shares physical displays. `subtype = "ard-high-performance"` is High Performance mode over RFB 003.889 and uses dynamically resizable virtual displays. The 003.889 handshake, type-30 authentication and wrap key, rekey, record layer, zlib, cursor cache, and metadata framing are also confirmed. |
| Protocol corrections | A dynamic descriptor's `max_width`/`max_height` are a fixed 3840×2160 backing ceiling, not the current mode. `AutoFrameBufferUpdate` does not make the tested server stream. A display record's fields are two bytes later than documented. A layout payload is two bytes shorter than its own length prefix says. `ViewerInfo`'s body carries numeric version triples rather than strings. High Performance reads the RFB pointer mask positionally — bit 2 is right and bit 3 is middle, the reverse of the RFB convention Standard mode honours. |
| Not implemented | Apple's High Performance controls for choosing one or two virtual displays and choosing among fixed resolution presets. |

## Confirmed display modes

`subtype = "ard"` is Apple Screen Sharing's Standard mode over RFB 3.8 and shares
the Mac's physical displays. `subtype = "ard-high-performance"` is Apple Screen
Sharing's High Performance mode over RFB 003.889 and uses virtual displays.

High Performance disables physical displays and moves all remote windows to the
virtual display. Apple's client supports up to two, arbitrary dynamic sizes, and
fixed presets when dynamic resolution is off. Remotex requests one, opening at
the pinned `width`/`height` when the config sets both and otherwise at the full
resolution of the client's own screen (named in the client's `connect`), at that
screen's density — which is how Apple's client opens, and it matters: the window
layout macOS produces depends on the opening size, and windows squeezed onto a
small opening display do not spread back out when it grows. With `resize = true`
a viewport report then sends a replacement full display configuration; the next
`AppleDisplayLayout` confirms its size.

```toml
[[targets]]
name = "macvirtualdisplay"
protocol = "vnc"
subtype = "ard-high-performance"
host = "..."
username = "sandbox2"
password = "qwertasdfg"
width = 1600
height = 1000
resize = true
```

### The display-configuration wire shape

The setup request is `SetDisplayConfiguration` (`0x1d`): a four-byte Apple message
header followed by one display descriptor and one mode entry. The header's `u16`
length counts the body only. The body begins with `u16 version = 1`, `u16
display_count = 1`, and `u32 flags = 0`.

The descriptor is `0x9c` bytes before its `0x1c`-byte mode table:

```text
+0x00 u16      descriptor size, including the mode table
+0x02 120B     opaque region
+0x7a u32      display_flags = 1
+0x7e u32      display_type = 4 (virtual display)
+0x82 f32 BE   physical width in millimetres
+0x86 f32 BE   physical height in millimetres
+0x8a u32      maximum backing width = 3840
+0x8e u32      maximum backing height = 2160
+0x92 u16      current mode index = 0
+0x94 u16      preferred mode index = 0
+0x96 u32      native full-dynamic rotations value = 7
+0x9a u16      mode count = 1
```

The `0x1c`-byte mode is `u32 width`, `u32 height`, `u32 scaled_width`, `u32
scaled_height`, `f64 refresh_rate_hz = 60`, and `u32 flags = 0`. `width`/`height`
are the render (backing) resolution and the scaled pair the logical one: a HiDPI
mode with `width = 2 × scaled_width` is honored — the measured 26.6 host created a
2x virtual display for a 1728×902-point mode (3456×1804 backing) and dropped back
to 1x when a later mode sent the pairs equal, matching what native Screen Sharing
requests from a Retina client. The answering layout reports the granted density in
its display record and under the combined `0xffffffff` `current_display` sentinel,
so a single-display layout's density is that display's, not the mixed-mosaic "no
single scale". `display_flags` bit 0 enables dynamic
geometry. Each viewport change resends the full descriptor with a replacement
mode, but the maximum fields stay at the native fixed 3840×2160 backing ceiling.
They are bounds on the virtual display, not another copy of the current mode:
putting the configured 1280×800 there made macOS accept arbitrary sizes through
1279×799 and decline 1281×600 or 1366×768 by answering with the old layout. With
the fixed ceiling, the same macOS 26.6 host accepted 1366×768, 1600×900 and
1920×1080 successively, then a ten-request arbitrary-size burst ending at the
last requested mode. The server calls `+0x96` rotations; `7` is Apple's captured
full-dynamic value, but its private bits remain unknown.

Apple's client UI may impose an 800×600 floor, but that is not a server protocol
limit on the measured host: the same 26.6 session accepted 799×599 exactly and
reported it in the answering layout. Remotex therefore does not clamp a viewport
that the server itself accepts.

The initial descriptor is always dynamic, even when `resize` is false. A reconnect
therefore re-enables the Mac's **Dynamic resolution** setting. `resize` controls
only whether remotex acts on later viewport reports.

Standard mode was independently remeasured July 31, 2026. After Apple DH auth,
`RFB 003.008` plus the same ten metadata encodings produced an unsolicited
`AppleDisplayLayout`. Selecting ids 4 and 1 produced 3200×1800 at 2× and
1280×800 at 1×. It uses the same display protocol without the 003.889 record
layer.

Standard mode compresses on the same terms as High Performance, remeasured
August 1, 2026. The second `SetEncodings` a layout triggers is honoured on the
plain 3.8 wire too: the Mac answered with another identical layout — so no display
state is lost — and switched to zlib rectangles. Over one identical 800×600
session it sent 3,380,550 bytes against 6,190,318 raw, with the decoded
framebuffer pixel-identical to a full repaint. remotex asks for zlib in both
subtypes; the gate that kept `ard` on raw pixels was removed.

Standard native pasteboard monitoring requires `ViewerInfo`, `SetMode(control)`,
then `AutoPasteboard(start)`. Without the first two, writes and explicit fetches
work but the Mac does not emit the `MiscStatus(cmd=2)` change notification.

With `clipboard = true`, the High Performance subtype uses the same native Apple
pasteboard messages in both directions. It sends the native cleartext `ViewerInfo`,
`SetMode(control)`, and `AutoPasteboard(start)` prelude before encryption setup. The
gateway repeats the idempotent `AutoPasteboard(start)` after the virtual display's
answering layout. The Mac reports changes with `MiscStatus(cmd=2)`;
`ClipboardFetch` and the zlib-compressed `ClipboardSend` archive carry the contents.
Each complete post-rekey client message is carried in an encrypted 003.889 record;
archive and session-id handling are shared with Standard mode.

Framebuffer responses and pasteboard messages share one ordered server stream. A
pasteboard status can arrive just after the gateway has requested the next update,
putting its fetch behind that one response. Once that response completes, remotex
does not issue another framebuffer request while the fetch remains pending; the
pasteboard reply (or an idle-gap recovery) therefore resumes polling before another
pixel response can get ahead of it. Repeated change statuses coalesce into one
follow-up fetch.

`AutoFrameBufferUpdate` is not a flow-control command. remotex only sends the
measured full-framebuffer arming at setup and after layouts; changing that rectangle
mid-session corrupts the live Mac's later updates.

### Picking a physical screen in Standard mode

For `subtype = "ard"`, `SetDisplayMessage` (`0x0d`) selects a physical display, and
the Mac confirms by
echoing its choice in the next layout's `current_display`:

| sent | `current_display` comes back | framebuffer becomes |
|---|---|---|
| `combine_all_displays = 1` | `0xffffffff` | 4480×1800 — the union |
| `display_id = 4` | `4` | 3200×1800 — that screen's own pixels |
| `display_id = 1` | `1` | 1280×800 |

The layout is authoritative; `src/vnc.rs` moves the checkmark only on confirmation.

### The density, and why picking a screen is what fixes it

Each display record carries **its own scale factor** as a big-endian `f64`: 1.0 for
the 1280×800 screen, 2.0 for the Retina one. It agrees exactly with the ratio of
that record's two bounds rects (3200/1600), so the two can be cross-checked.

**A combined framebuffer has no single density.** Here 4480×1800 combines a 1×
1280×800 display and a 2× 3200×1800 display across 2880×900 points. The header
ratio, 4480/2880 = 1.56, represents neither display. `Layout::scale` therefore
returns `UNSCALED` for the combined view and the display's scale after selection.

## The other corrections

### The first `SetEncodings` decides whether displays are reported at all

The behavior is unexplained. The list in
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

Reversal and duplication failures rule out simple set-membership and capability
tests. Sixteen variants used fresh connections, with no exceptions in either
direction. The server's interpretation is a revision gap.

Two consequences for an implementer:

- **zlib cannot be in the first `SetEncodings`.** Send a second one, with zlib
  appended, once a layout has arrived: the Mac keeps its display state and simply
  changes encoder. Measured at 398 KB for a 3200×1800 frame against 23 MB raw. This
  is what `ENCODINGS_WITH_ZLIB` and the `asked_for_zlib` flag in `src/vnc.rs` are for.
- **Advertising is a promise.** Every entry in that list has to be decodable or at
  least steppable, and two of them do not share the common length rule: `CursorPos`
  (`0x44c`) has no payload at all, and `DisplayInfo` (`0x44d`) is four `u16`s then
  `0x1c` bytes per screen. `LastRect` has to actually end the update, which means
  handling the `0xffff` rectangle count that goes with it. (`UserInfo` (`0x44e`) — a
  counted name then a counted image — is *not* advertised but is decoded anyway, on
  the same grounds as `DeviceInfo`: tolerating an encoding that turns up unasked
  costs nothing, and desyncing on it costs the session.)

Qualification: the probe used eleven entries, while the shipped list has ten and
omits `UserInfo`. That difference was not among the sixteen probe variants. The
shipped list produced layouts twice through the gateway, so "any single removal
fails" has one known exception and the required subset remains unknown. Leave the
shipped order and contents unchanged.

### A layout payload is two bytes shorter than it declares

The `u16` prefix counts the whole payload including itself — `0x14 + displays ×
0x38`, which is 132 for two screens and 76 for one — but **two fewer bytes are
sent**. The final display record stops after its last field and omits its two
trailing pad bytes.

Consuming the declared count steals two bytes from the next message. The following
framebuffer update then reads width `0x0c80` (3200) as its rectangle count and later
reports pixel bytes `0xdaffdada` as an encoding. This appears on the second layout,
so reproduction requires a display switch. Both the independent probe and gateway
reproduced it.

### A display record's fields are two bytes later than documented

The `f64` `3ff0000000000000` (1.0) appears at `+0x02` and `+0x0a`, not the
offsets `+0x00` and `+0x08` documented in §8.4. Those offsets yield
`display_id = 0` for every screen and invalid scales.

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

### High Performance reads the pointer mask as CGMouseButton numbers

The RFB convention is mask bit 1 = left, bit 2 = middle, bit 3 = right, and
Standard mode honours it. High Performance's agent reads the same mask
positionally instead — bit 2 = *right*, bit 3 = *middle*, matching CGMouseButton
numbering (left 0, right 1, center 2). Measured on macOS 26.6 by holding each
button through a live session of each subtype and reading
`CGEventSource.buttonState(.combinedSessionState)` on the Mac over SSH: mask
`0x04` lands as button 2 (middle) in High Performance and as button 1 (right) in
Standard; `0x02` the reverse; `0x01` and pointer motion agree in both. A
by-the-book right-click therefore arrived on the virtual display as a
middle-click — the button macOS does nothing visible with — which presented as a
right button that never opened a menu in High Performance mode, session after
session, while left click and motion worked. `Buttons` in `src/vnc.rs` swaps the
two bits for this subtype alone; after the swap, three fresh sessions opened a
context menu on nine of nine right-clicks, confirmed against the Mac's own
window list (a context menu is a window at the pop-up-menu layer, 101).

The wheel bits (4–7) were not re-measured: proportional scrolling on them
predates this correction and works, so whatever the agent reads them as agrees
with RFB in effect. The native client's own input path is `0x10`
EncryptedInputEvent, not the plain RFB PointerEvent — presumably why Apple never
noticed the plain path's ordering.

### `AutoFrameBufferUpdate` (`0x09`) does not make the server stream — §8.11, R-A16b

The document says it "switches the server to server-driven framebuffer streaming"
and that "after sending this, a client SHOULD NOT continue to poll".

macOS 26 does not stream. Armed or not, it answers a non-incremental
`FramebufferUpdateRequest` and is otherwise silent — including while the screen is
visibly changing. Measured by sending pointer events on a 2.5-second cycle and
never re-requesting: **zero rectangles in 25 seconds.** The same cycle with a
non-incremental request appended returns a full update every time.

A client that follows the document paints one frame and then freezes. **Keep
polling.** Sending the measured full-framebuffer `0x09` is what keeps cursor updates
alive across a login or lock transition, which is why this implementation still
sends it at setup and after a layout — but it is not the update driver, and its
rectangle is not changed as a flow-control mechanism.

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
does not do that. The layout arrives with or without it. A live High Performance
probe that sent `AutoPasteboard(start)` in the cleartext native prelude emitted
`MiscStatus(cmd=2)` after the Mac pasteboard changed; sending the enable only inside
the record layer did not. The gateway therefore enables it before encryption and
repeats it after the answering virtual-display layout.

### The metadata encodings also arrive as bare messages

`0x451` also comes as message type `0x51`, `0x453` as `0x53`, and so on: the message
type is the encoding's low byte, with the same `u16`-length framing. A live session
sends both forms of the same content. There are also two zero-payload message types,
`0x04` (ServerAck) and `0x07` (NOP).

A client that does not tolerate these ends the session on the first bare message,
typically `0x04` a few seconds after connection.

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

## Reproducing any of this

The probe was throwaway Python speaking the protocol by hand — deliberately not
calling into `src/`, so a misreading on one side could not be agreed with by the
other. It lived at `tmp/apple889_probe.py` (gitignored). The shape is: TCP to port
5900, `RFB 003.889\n` both ways, security type 30, the DH exchange above, ClientInit
`0xC1`, ServerInit, `SetEncodings`, `SetPixelFormat`, `SetEncryption(1)` and `(2)`,
read the rekey, then a record layer as specified above around ordinary RFB.

The pointer-mask measurement used two later probes, gitignored the same way:
`tmp/input_trace_probe.py` drives the gateway WebSocket and reads
`CGEventSource.buttonState` on the Mac over SSH while each button is held, and
`tmp/right_click_probe.py` right-clicks the desktop across fresh sessions and
asks the Mac's window list whether a pop-up-menu-layer window appeared. Both
lean on small Swift tools compiled under `~/probe` on the sandbox Mac.

Three instruments did the work, and two of them were outside the protocol:

1. **Enumerating displays over SSH with `CGGetActiveDisplayList` while a Standard
   session was live.** This validated the physical display ids and geometry used to
   check `AppleDisplayLayout`.
2. **Bisecting one message or one encoding at a time, on a fresh connection each
   time.** This mapped the `SetEncodings` behaviour. It only works with every other
   session closed; stale sessions invalidate display-state observations.
3. **A rolling log of every byte handed upward, dumped on the first parse failure.**
   Framing bugs here surface many messages after their cause; nothing else would
   have found the two-byte layout length.
