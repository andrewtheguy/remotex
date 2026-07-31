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

The implementation is `src/vnc_record.rs` (the transport), `src/vnc_apple.rs` (the
messages), and the `Dialect::Apple889` path in `src/vnc.rs`.

## Summary

| | |
|---|---|
| Confirmed | The handshake, type-30 authentication and its wrap key; the rekey; the record layer in full; `SetDisplayConfiguration`; zlib; the cursor cache; the metadata encodings. |
| **Wrong** | `ViewerInfo` must not be sent. `AutoFrameBufferUpdate` does not make the server stream. |
| **Missing** | A 003.889 session replaces the Mac's displays with one synthesized display, so there is nothing to enumerate or pick — and no `AppleDisplayLayout` ever arrives. |

## Corrections

### `ViewerInfo` (`0x21`) must not be sent — §5.5, §5.4, R-A7

The document lists it as a required prelude item and frames its body as `u16
version`, `u16 viewer_app`, *version strings*, `byte[32] capability_bitmap` —
without ever saying how the strings are framed.

There is no shape that works. macOS reads **more bytes for the message than its own
`body_len` field declares**, so whatever follows is consumed as the tail of it: the
`SetEncryption` behind it disappears, the server waits for the rest of a message
that has already been sent, and **the rekey never arrives**. Neither end reports an
error. The session simply stops after ServerInit with a connected socket and
silence, which is the least diagnosable failure in the whole protocol.

Measured, each on its own connection:

| prelude | result |
|---|---|
| `SetEncryption(1)` alone | **rekey, immediately** |
| `ViewerInfo` → `SetEncryption(1)` | silence |
| `ViewerInfo` → `SetEncryption(1)` → `SetEncryption(2)` | silence |
| `ViewerInfo` → `SetEncryption(1)` → `SetMode` → `SetEncryption(2)` | silence |
| `ViewerInfo` with two zero-length strings before the bitmap | silence |

**Send `SetEncryption(1)` and `SetEncryption(2)` and nothing else.** Nothing is
lost: the only bit the document says the server reads out of a `ViewerInfo` gates
observe-only mode. `SetMode` is genuinely optional, as documented.

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

### ServerInit's name field is not a name — §5.3

The length is correct and the stream stays in sync, so this costs nothing but a
confusing log line. The field is 48 bytes for a Mac called "Andrew's Virtual
Machine", and contains 22 bytes of binary before the UTF-8 name:

```
+0x14  00 00 00 30                                    u32 name_len = 48
+0x18  00 00 00 00 00 52 bf f6 e7 2f ec 00 00 00 …    22 bytes, purpose unknown
+0x2e  41 6e 64 72 65 77 e2 80 99 73 20 …             "Andrew’s Virtual Machine"
```

Treat it as opaque. Do not print it as a string without saying so.

## The display finding, which is the important one

**A 003.889 session does not share the Mac's screens. macOS creates a new display
for it and removes the real ones for the session's duration.**

Enumerated with `CGGetActiveDisplayList` over SSH on a Mac with two distinct,
non-mirrored displays (1280×800 at (0,0) and 1600×900 at (1280,0)):

| | `CGGetActiveDisplayList` says |
|---|---|
| no session | `active: 2` — id 1 at 1280×800, id 4 at 1600×900 |
| plain RFB 3.8 session (`subtype = "ard"`) | `active: 2` — unchanged |
| **003.889 session** | **`active: 1` — one display at 4480×1800** |

The synthesized display's `CGDirectDisplayID` increments with every session
(observed 10, then 11, then 12), so it is created fresh each time and destroyed on
disconnect. It is not a mirror of anything (`CGDisplayIsInMirrorSet` is false) and
its size matches neither real display nor their bounding box (2880×900); 4480×1800
is consistent with the second display at 2× beside the first at 1×, but that is
inference, not measurement.

This is **not** caused by anything the client sends. It happens with:

- `SetDisplayConfiguration` omitted entirely;
- `display_type` = 0, 2 or 4;
- the dynamic-resolution flag set or clear;
- ClientInit `0x01` as well as `0xC1`.

So it is a property of the 003.889 session itself, established before the first
client message after ServerInit.

Everything else about display handling follows from it:

- **No `AppleDisplayLayout` (`0x451`) ever arrives**, with or without it advertised
  in `SetEncodings`, under every descriptor above. There are no real screens during
  the session for it to describe.
- **`SetDisplayMessage` (`0x0d`) is ignored.** Tested with each real display's
  `CGDirectDisplayID` and with `combine_all_displays = 1`: the framebuffer stays the
  synthesized display's size in every case. There is only one display to pick.

### What this means for picking a screen

**To see a Mac's real displays, use plain `subtype = "ard"`** — RFB 3.8 leaves them
alone and shares them as one framebuffer spanning all of them. That is the
workaround, and it is what this gateway recommends.

How Screen Sharing.app offers its *Both Displays / Display 1 / Display 2* menu is
unresolved. It is not the mechanism this document describes, because that mechanism
demonstrably produces no display list on macOS 26. Settling it needs a packet
capture of Screen Sharing.app itself; nothing in the reverse-engineered document is
a substitute, since this is precisely where it is wrong.

`src/vnc_apple.rs` still parses `0x451` and still sends `0x0d`, unit-tested against
synthetic payloads built from the document's field model. That code is dormant: it
would light up if a Mac ever sent a layout, and it costs nothing while none does.
Do not read its presence as evidence that picking works.

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
rectangle is ~628 KB against a 65 520-byte record ceiling, so it spans about ten
records on the *first* update of every session. A parser that treats one record as
one message desyncs immediately — the first thing that happened to the probe used
here.

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

**`SetDisplayConfiguration` (`0x1d`).** The length rules are right —
`message_size` = total − 4, `display_info_size` = `0x9c + mode_count × 0x1c` — and a
bare static descriptor (no dynamic flag, `display_type = 0`, zeroed mode indices,
one mode entry) is accepted without complaint. It has no observable effect on this
Mac, which synthesizes its display regardless.

**zlib (`0x06`).** `u32 length` then a chunk of **one deflate stream for the life of
the connection**, inflating to exactly `w × h × 4`. Confirmed with an independent
inflater: 628 232 → 32 256 000 bytes on the first frame, and correct on every frame
after, which only holds if the sliding window carries across rectangles. The ratio
is the reason to use this subtype at all — roughly 50:1 on a static desktop.

**The cursor cache (`0x450`).** STORE when `compressed_len > 0`, SELECT when zero,
and the payload really is a `w·h·4` BGRA pixmap followed by a **separate** `w·h`
alpha plane — folding the fourth pixel byte in as alpha produces a uniformly opaque
cursor. Real cursors arrived and rendered.

**The metadata encodings** `0x453`, `0x455`, `0x456`. All three frame themselves the
same way — a `u16` giving how much follows — so one rule steps over all of them
without desyncing. They arrive only when advertised in `SetEncodings`.

## Still unknown

- How Screen Sharing.app enumerates displays, and whether any client can pick one
  on this wire. **The open question.**
- Whether the synthesized display can be influenced at all — its size, or whether
  the real screens can be kept.
- `ViewerInfo`'s real framing.
- Apple's still-image codecs `0x3ea` and `0x3f3`; the document leaves the first's
  rectangle body and the second's command-code table unresolved, and neither was
  advertised here, so nothing was learned.
- The Adaptive media path (`0x1c`, SRTP, HEVC): not attempted.
- Authentication types 33, 35 and 36: not attempted, type 30 being sufficient.
- Multi-rekey, and whether sequence counters survive a second one.

## Reproducing any of this

The probes were throwaway Python speaking the protocol by hand — deliberately not
calling into `src/`, so a misreading on one side could not be agreed with by the
other. Rebuilding one is a few hours' work and the shape is: TCP to port 5900,
`RFB 003.889\n` both ways, security type 30, the DH exchange above, ClientInit,
ServerInit, `SetEncryption(1)` and `(2)`, read the rekey, then a record layer as
specified above around ordinary RFB.

The two instruments that mattered were both outside the protocol: enumerating
displays over SSH with `CGGetActiveDisplayList` *while a session was live* (which is
what found the synthesized display, and which no amount of protocol reading would
have), and bisecting the prelude one message at a time on a fresh connection each
time (which is what found `ViewerInfo`).
