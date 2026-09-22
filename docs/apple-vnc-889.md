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

Everything below is
measurement, not specification: Apple documents none of RFB 003.889, and the
confirmations here hold for the Macs and the macOS version named above rather
than for the protocol. A macOS update is free to invalidate any row. The
dynamic-resolution descriptor has been exercised against the arbitrary-size
boundary and a burst of viewport reports, but remains reverse engineered. The
system audio below is **experimental** and stays behind the non-default
`apple-hp-audio` feature.

| | |
|---|---|
| Confirmed | `subtype = "ard"` is Apple Screen Sharing Standard mode over RFB 3.8 and shares physical displays. `subtype = "ard-high-performance"` is High Performance mode over RFB 003.889 and uses dynamically resizable virtual displays. The 003.889 handshake, type-30 authentication and wrap key, rekey, record layer, zlib, cursor cache, and metadata framing are also confirmed. |
| Protocol corrections | A dynamic descriptor's `max_width`/`max_height` are a fixed 3840×2160 backing ceiling, not the current mode. `AutoFrameBufferUpdate` does not make the tested server stream. A layout's length prefix counts only what follows it, and a `u16` display count precedes the records. `ViewerInfo`'s body carries numeric version triples rather than strings. High Performance reads the RFB pointer mask positionally — bit 2 is right and bit 3 is middle, the reverse of the RFB convention Standard mode honours. |
| Fractional ratios | A virtual display mode whose backing/scaled ratio is not 1 or 2 is not rounded by the Mac. Measured August 23, 2026 on macOS 26.6.2: 2561×1440 backing over 1707×960 scaled (1.5x) created 1707×960 points at 2x, 2880×1800 over 1920×1200 (1.5x) created 960×600 points at 2x, and 2560×1440 over 2048×1152 (1.25x) created 960×540 points at 2x — a desktop whose text looks zoomed while the Dock, shrunk to fit the width, does not. Remotex therefore asks only for 1x or 2x (`protocol::render_density`). |
| Lingering display | The virtual display outlives its session: a reconnect within a few seconds found it still there (the new session's ServerInit reported the previous mode and the display kept its id), and one after 45 s found the Mac back on its 800×600 physical display with a fresh id. The new session's own layout arrives either way, including when the requested mode equals the lingering one. |
| Pre-rekey messages | `MiscStatus` (`0x14`) can arrive in the cleartext window between `SetEncryption` and the rekey, especially after a server restart when the Mac has stale clipboard state. The client must tolerate it during `await_rekey`. |
| Not implemented | Apple's High Performance controls for choosing one or two virtual displays and choosing among fixed resolution presets. |
| Authentication | Remote Management's default "All users" setting rejects valid account credentials with the same error as an incorrect password. Add the account to the per-user access list with Observe and Control before treating the failure as a protocol fault. |

## Remote Management access

Rule out the Mac's Remote Management permissions before treating an
authentication failure as a protocol fault. When the account lacks permission,
the type-30 exchange completes and the wrap key is derived before the Mac sends
a failing `SecurityResult`, just as it does for an incorrect password. Both
Apple subtypes report `VNC authentication failed: <the Mac's reason>`, which
does not distinguish between the two causes.

Remote Management's default **All users** setting rejects valid account
credentials. Add the account to the per-user access list and grant at least
**Observe** and **Control**. The Mac records this selection as
`ARD_AllLocalUsers = 0` in
`/Library/Preferences/com.apple.RemoteManagement`.

Configure these permissions under System Settings → General → Sharing → Remote
Management (ⓘ) → the account. Over SSH, this command enables the account and
grants all Remote Management privileges:

```sh
sudo /System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/Resources/kickstart \
  -configure -access -on -users <account> -privs -all -restart -agent
```

The **VNC viewers may control screen with password** setting is for clients that
use RFB security type 2. Remotex authenticates with type 30 and the account's
own password, so this legacy option does not need to be enabled.

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
Mac can respond to `AutoPasteboard(start)` with a `MiscStatus(cmd=2)` in the
cleartext window before the rekey arrives — particularly after a server restart,
when stale clipboard state from the previous session triggers an immediate
notification. The client must tolerate it during `await_rekey` rather than
treating it as a protocol error. The
gateway repeats the idempotent `AutoPasteboard(start)` after the virtual display's
answering layout. The Mac reports further changes with `MiscStatus(cmd=2)`;
`ClipboardFetch` and the zlib-compressed `ClipboardSend` archive carry the contents.
Each complete post-rekey client message is carried in an encrypted 003.889 record;
archive and session-id handling are shared with Standard mode.

Framebuffer responses and pasteboard messages share one ordered server stream. A
pasteboard status can arrive just after the gateway has requested the next update,
putting its fetch behind that one response. Once that response completes, remotex
pauses normal incremental framebuffer polling while the fetch remains pending. A
layout-required non-incremental full-repaint request may still be sent during that
pause. The pasteboard reply, or an idle-gap recovery that preserves any outstanding
full repaint, then resumes incremental polling. Repeated change statuses coalesce
into one follow-up fetch.

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

### Which encodings make the Mac report its displays

`screensharingd` resets its display flags on every `SetEncodings` and sets one for
`DisplayInfo` (`0x44d`) and one for `AppleDisplayLayout` (`0x451`) wherever they
appear in the list. Measured on macOS 26.6 with `tests/hp_audio_probe.py
--first-encodings`:

| first `SetEncodings` | the Mac sends |
|---|---|
| `vnc_apple::ENCODINGS` | the layout |
| the same plus zlib | the layout |
| the same, reversed | the layout |
| zlib, Raw, rekey, cursor cache, `0x44d`, `0x451` | the layout |
| without `0x451` | `DisplayInfo` |
| without `0x44d` | nothing about its displays |

Order matters only for the preferred codec, the first of zlib, ZRLE and Apple's
own codecs listed; LastRect is recognised nowhere. Every `SetEncodings` that lists
`0x44d` produces another layout. An earlier table here, which found that any
change to the list cost the layout, was measured with a layout reader four bytes
out of step.

`vnc_apple::ENCODINGS_WITH_ZLIB` is sent once a layout has arrived; the Mac keeps
its display state and switches encoder, measured at 398 KB for a 3200×1800 frame
against 23 MB raw.

**Advertising is a promise.** Every entry in the list has to be decodable or at
least steppable, and two of them do not share the common length rule: `CursorPos`
(`0x44c`) has no payload at all, and `DisplayInfo` (`0x44d`) is a `u16` width and
height, a `u32` of flags and a `u16` count, then `0x1c` bytes per screen.

### A layout's length counts what follows it

The `u16` prefix counts the bytes **after** itself — `0x14 + displays × 0x38`,
which is 132 for two screens and 76 for one — and that many are sent. Between the
header and the records sits a `u16` display count, which §8.4 does not have.

An earlier reading counted the prefix in its own length and started the records two
bytes early. Every field then looked two bytes late, the last four bytes of the
last record — zero, always — were left on the stream, and a reader that took them
as an empty framebuffer update came through intact. Consuming the declared count
under that reading stole two bytes from the next message instead, and the session
died a few messages later on a rectangle count that was really a screen width.
`ScreensharingAgent`'s encoder and Apple's own viewer settle the framing — see
[the binary audit](apple-vnc-889-binary-audit.md#the-display-layout).

### A display record, as sent

Both bounds rects are **`(top, left, bottom, right)`**, not the `(x, y, w, h)` the
document models; a size is a difference of edges. The record, `0x38` bytes:

```text
+0x00 f64 BE   this screen's scale factor    -- 1.0 or 2.0; 0.0 if the mode lookup failed
+0x08 f64 BE   viewer scale factor           -- the daemon's own scaling, 1.0
+0x10 u32 BE   display_id (CGDirectDisplayID)
+0x14 rect     logical bounds  (u16 top, left, bottom, right)
+0x1c rect     backing bounds  (u16 top, left, bottom, right)
+0x24 u32 BE   flags: bit0 = main, bit1 = in a mirror set, bit2 = dynamic virtual display
+0x28 16B      pixel format (bpp, depth, big-endian, true-colour, maxes, shifts, pad)
```

And the header, which is 0x14 bytes after the length prefix:

```text
+0x00 u16  version = 5
+0x02 u16  logical width  -- the whole desktop, in points; does not change on a selection
+0x04 u16  logical height
+0x06 u16  backing width   -- THE FRAMEBUFFER, and what does change on a selection
+0x08 u16  backing height
+0x0a u32  current_display, 0xffffffff for the combined view
+0x0e u32  session state: 0x04 on console, 0x01 obscured, 0x02 locked, 0x10 login pending
+0x12 u16  display count
```

Ground truth these offsets reproduce, measured separately over SSH: ids 1 and 4,
1280×800 at (0,0) and 1600×900 at (1280,0), the first one main, the second Retina.
`src/vnc_apple.rs` pins a captured payload against exactly that.

`CGDisplayIsInMirrorSet` is true of every member of a mirror set, the one the others
copy included, so bit 1 marks the original as well as its copies. Members share an
origin; the gateway offers the first of them.

### ServerInit's name field is not a name

It is 22 bytes of structure and then the name:

```text
+0x00  u16   zero
+0x02  u32   server flags
+0x06  16B   capability bitmap
+0x16  ...   the UTF-8 name
```

Flags: `0x01` observe only, `0x02` may-control, `0x04` session-select, `0x08` screen
capture not permitted, `0x10` always set, and bits 5 and up the most virtual
displays the Mac will create. The test VM reads `0x00000052` — may-control and two
virtual displays — and its name comes out as `"Andrew's Virtual Machine"`, which
printing the whole field as latin-1 turned into mojibake.

`0x04` follows from the ClientInit byte: `0x80` asks for this enhanced ServerInit,
and `0x40` asks a Mac whose console user is not the one authenticated to have the
viewer choose a login session first, in an exchange that follows ServerInit. Apple's
viewer sets `0x40` only when it has a session picker to offer; remotex has none and
sends `0x81`. See [the binary audit](apple-vnc-889-binary-audit.md#serverinits-flags).

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

The mapping follows the protocol version, not the mode: `screensharingd` swaps
mask bits 1 and 2 for every viewer except 3.888 and 3.889, and the agent always
reads the mask positionally. A Mac scrolls only on a mask of exactly `0x08` or
`0x10`; any other mask, a wheel bit with a held button or the horizontal `0x20` and
`0x40` included, is posted as buttons by bit position. So remotex sends each
vertical pulse alone and no horizontal ones — see
[the binary audit](apple-vnc-889-binary-audit.md#the-pointer-mask). The native
client's own input path is `0x10` EncryptedInputEvent, which carries all four
wheel directions and a click count.

### Double-click is chained by the Mac, at a login-time threshold

A plain RFB PointerEvent carries no click count, so the Mac decides which
presses chain into a double-click: `ScreensharingAgent` stamps `clickState` on
the events it posts. Measured with a passive event tap on macOS 26.6: two
identical-coordinate clicks with no motion between them chain when the
down-to-down gap is under the session's double-click window and stay two singles
when it is not, with the cutoff sitting exactly at the machine's
`com.apple.mouse.doubleClickThreshold`.

That window is seeded **once at login**. Writing the preference, moving the
System Settings slider, and restarting `screensharingd` or `ScreensharingAgent`
all left the live window unchanged; only a logout or reboot re-seeds it. The
native client is immune — its EncryptedInputEvent path chains clicks itself and
chained at gaps well past the same session's window. So a Mac that
double-clicks fine in Apple's client but not through remotex has a stale or
too-fast threshold on the Mac itself: set it to a sane value
(`defaults write -g com.apple.mouse.doubleClickThreshold -float 0.5`) and
reboot. remotex forwards the clicks as they happened and adds no compensation.

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

### The metadata encodings arrive only as rectangles

Every metadata item — the layout, vendor keysyms, keyboard source, `DeviceInfo` —
is a one-rectangle framebuffer update. `screensharingd` has no other way to send
one, and Apple's viewer closes the connection on a server message type outside 0–3,
`0x14`, `0x15`, `0x1e`, `0x1f`, `0x20`, `0x22`, `0x23` and `0x51` (which is
`SystemInfoData`, with a `u32` length). The bare `0x51`/`0x53`/`0x55`/`0x56`,
`ServerAck` `0x04` and `NOP` `0x07` once recorded here were a reader two bytes out
of step inside those rectangles.

### The numbers, in both forms

Apple writes its encodings in hex and the media stream's in decimal, while the wire
and RFB's registry count in decimal throughout — so the same code is searched for
two ways. Both are given here, and `src/vnc.rs` logs an unexpected encoding as
`1105 (0x451)` for the same reason. A pseudo-encoding is negative and only ever
written in decimal.

| encoding | hex | decimal | |
|---|---|---|---|
| `CursorPos` | `0x44c` | 1100 | |
| `DisplayInfo` | `0x44d` | 1101 | |
| `UserInfo` | `0x44e` | 1102 | not advertised |
| rekey | `0x44f` | 1103 | |
| cursor cache | `0x450` | 1104 | |
| `AppleDisplayLayout` | `0x451` | 1105 | |
| vendor keysyms | `0x453` | 1107 | |
| keyboard source | `0x455` | 1109 | |
| `DeviceInfo` | `0x456` | 1110 | not advertised |
| `kSSVideoEncoding_AVCMediaStream` | `0x3f2` | 1010 | all media-stream replies |
| zlib | `0x06` | 6 | standard RFB |
| Raw | `0x00` | 0 | standard RFB |
| `DesktopSize` | — | -223 | pseudo-encoding |
| `LastRect` | — | -224 | pseudo-encoding |

The message types this client steps over or sends: `MiscStatus` `0x14` (20),
`RFBMediaStreamServerConfiguration`
`0x1c` (28), `AutoFrameBufferUpdate` `0x09` (9), `ViewerInfo` `0x21` (33).

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
Only ever one per session, so multi-rekey remains unexercised. The Mac may send
`MiscStatus` (`0x14`) in the cleartext window between `SetEncryption` and the
rekey; the client must step over it rather than bailing on it.

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

## The media stream: High Performance system audio

High Performance carries the Mac's **system audio**, and it does not ride RFB at
all. The agent (`ScreensharingAgent`'s `SSUDPSender`) opens an AVConference — the
FaceTime media stack — `AVCAudioStream` over **UDP with SRTP** straight to the
viewer. RFB only negotiates it. The gateway implements this in
`src/vnc_apple_audio.rs` (the wire, always built) and `src/aac_eld.rs` (the
decoder, behind the non-default `apple-hp-audio` feature); the offers Apple's
client generated are rebuilt there field by field and checked against the
captured bytes. This was measured on macOS 26.6.2 with a
throwaway Python client ([`tests/hp_audio_probe.py`](../tests/hp_audio_probe.py)) that speaks the
whole 003.889 wire by hand and never calls into `src/`; it **negotiated and
decrypted 1,794 live audio packets** from a Mac that had sound playing. None of
the mechanism below is documented by Apple.

**The negotiation is one client message and up to three server reply types.** A
successful negotiation gets message 1 (the ports) and message 2 (the answer); message
3 (an error) is the alternative terminal reply. After the
first display layout, the client advertises encoding **1010** (`0x3f2`,
`kSSVideoEncoding_AVCMediaStream`) in a second `SetEncodings` and sends message
type **`0x1c`** (`RFBMediaStreamServerConfiguration`, version 3) inside the record
layer:

```text
+0x00 u8   0x1c
+0x01 u8   pad
+0x02 u16  body length (everything after this field)
+0x04 u16  version = 3
+0x06 u32  flags = 0
+0x0a u16  audio offer length
+0x0c u16  video1 offer length
+0x0e u16  video2 offer length
+0x14 16B  session UUID
+0x24 46B  audio SRTP key, viewer -> server
+0x52 46B  audio SRTP key, server -> viewer
+0x80      audio offer, then 46B v1 key v2s + 46B v1 key s2v + video1 offer
           (video2 likewise, when present)
```

The server answers with framebuffer rectangles, not record-layer messages, all
using encoding **1010** (`0x3f2`). Message 1 is `u16 type, u16 version, u32
flags, u16 audio UDP port` — audio at that port, video1 at port+1, video2 at
port+2. Message 2 is the AVC answer with the same three offer lengths at `+0x0a`.
Message 3 is a media-stream error with `u32 errorType, u32 subCode`.

**The native viewer configures the media stream once, then enables dynamic
resolution only after media setup completes.** In the x86_64 Screen Sharing
framework, `RFBMediaStreamServerConfiguration` is called from
`-[SSSession stConfigureServerMediaStream]`; no resize or display-layout path
calls it again. `-[SSSessionView ssSessionReady:]` explicitly defers switching
to dynamic resolution until `avcMediaSessionReady` when AVC setup is pending.
That callback starts the video and audio streams before it switches dynamic
resolution on. Remotex does not follow that ordering, because the stream it would
start is stopped again by the first resize, from the full-screen display a session
opens on to the window's size. It sends the window's size once the first layout
has arrived, and its first `0x1c` once that resize has settled. The Mac accepts a
`SetDisplayConfiguration` before any media stream is configured.

**`screensharingd` tears down the RTP sender on every display change.** The
native viewer's AVConference keeps the transport alive internally, so it never
re-sends `0x1c`. Remotex has no AVConference — it decrypts raw SRTP — so it
re-sends the offer (same keys, updated video size) and restarts the receiver
after each layout change. The Mac answers with message 1 (same ports), message 2
(accepted), and a fresh SSRC.

### Resizing a High Performance display, as measured

Measured September 22, 2026 on macOS 26.6.2 from the Mac's unified log while the
gateway resized its virtual display:

- **A display change stops the audio.** `SetDisplayConfiguration` takes the
  agent's `udpSenderCR` lock — the one `SetServerStreamConfiguration` (`0x1c`)
  holds while it builds a stream — then calls `AVCAudioStream stop` before the
  mode changes. The Mac sends message 1 by itself once the change finishes, but
  starts no stream until it gets an offer. An offer made mid-resize holds that lock
  against the next change: 2.8 s and 5 s waits were logged. Remotex offers once the
  resize has settled, as Apple's client appears to.
- **Overlapping and oversized reads crash the agent.** `ScreensharingAgent` died
  with `EXC_BAD_ACCESS` in `_platform_memmove` under
  `agent_SSAgent_ReadScreenDataIntoSharedMemory_rpc` — the `memcpy` from the
  capture `IOSurface` — within a second of a change that shrank the display, while
  a pixel request sized for the old display was being served. Every earlier agent
  crash report on the test Mac has the same stack. `screensharingd` logs the
  failed RPC as `(ipc/mig) server died`, relaunches the agent on the physical
  1280×800 display, and the session loses its virtual display, sound and, when a
  second request is in flight, its connection. The region read is not only a
  pixel request's. The one `AutoFrameBufferUpdate` (`0x09`) armed is served on
  every captured frame, and the change produces one. A layout re-arms it at the
  full size, so every change after the first met a full-size armed region. A
  2x→1x change of the same points, a quarter of the pixels, crashed the agent in
  each of three tries, and smaller shrinks survived by chance. So the gateway sends
  a change only at the end of an update, when no full-size pixel request is
  outstanding. It re-arms `0x09` for the one pixel at the origin, which every mode
  has, just ahead of the change, and until the answering layout it polls with an
  incremental request for that pixel. It never has two changes out. With the
  re-arm, 2x→1x changes and shrinks went through without a crash. Polling that
  pixel with *full* requests every 200 ms crashed the agent again, because each one
  is a read.
- **The Mac reads nothing while it is writing an update.** `screensharingd`'s
  update sender holds the viewer's lock while it deflates a rectangle and waits
  for the socket to take it, and the connection thread needs the same lock to read
  the next client message. A forced full update of a 2x display is several
  megabytes of zlib, so a client that drains the socket slowly leaves every message
  it sends unread until that update is through. A debug-build gateway reads about
  2 MB/s: a `SetDisplayConfiguration` sent right after a change to 2x sat
  acknowledged by the Mac's kernel but unread for 20 to 30 seconds, until the Mac
  had finished pushing the repaint. A release build drains the same traffic in
  well under a second, and every change, 2x to 1x included, is answered in about
  2.5 seconds. A sample of `screensharingd` in the stall shows the connection
  thread and the main thread's timer blocked on one mutex, and the thread holding
  it in `deflate` and `kevent`.

**The offer is a binary plist wrapping a protobuf**, produced by
`AVCMediaStreamNegotiator` (`initWithMode:8` for audio, `7` for the screen video):
`{ avcMediaStreamNegotiatorMode, avcMediaStreamOptionCallID (UUID string),
avcMediaStreamOptionRemoteEndpointInfo (protobuf: model, build),
avcMediaStreamNegotiatorMediaBlob (zlib of a protobuf: SSRC, "Viceroy 1.7.0", a
codec list) }`. A reimplementation that does not link AVConference must synthesize
these bytes itself. `tmp/probe2.m` (gitignored) generates them on the Mac; a mode-7
video offer needs `Video{Width,Height,Resolution}` and `TransportProtocolType`
options set.

**The Mac refuses audio alone.** A `0x1c` message with `video1 offer length 0`
negotiates the audio successfully (`AVCAudioStream-configure didSucceed=1`) and
then tears the whole stream down — `unable to create video config`, error type 2 —
because `startAVCMediaStreams` fails the video1 leg and aborts both. A **valid
mode-7 video offer has to ride alongside the audio**, even though this gateway has
no use for the HEVC screen the video port would carry (the zlib rectangles already
carry the picture). So the audio path structurally pulls in the HEVC video
negotiation.

**What the Mac then sends** (confirmed from the agent's own `AVCAudioStream
configure:` log and from the decrypted packets):

- **Codec type 16 = AAC-ELD**, 48 kHz, stereo, 320 kbit/s, RTP payload type 101,
  `ptime` 10 ms. One RTP packet per 480-sample frame; measured timestamps advance
  exactly 480 per packet.
- **RTCP once a second, timeout 3 s.** A viewer that never sends RTCP has its
  stream stopped by the agent.
- **SRTP AES-256 counter mode.** RFC 3711 key derivation from the 46-byte master
  (32-byte key, 14-byte salt); per-packet IV is `(salt << 16) XOR (ssrc << 64) XOR
  (packetIndex << 16)`. Decrypting the captured packets with the key the client
  itself sent produced structured AAC-ELD: 1,423 of 1,794 frames share the
  `0x89ffffff` near-silence prefix, which random output from a wrong key never
  does. The client binary sets `SRTPCipherSuite = 5`
  (`AES_256_AUTH_NONE`), yet the agent's negotiated config logged suite **7**
  (`AES_256_AUTH_SHA1_80`, a 10-byte auth tag appended to each packet). Both
  readings decrypt the same leading bytes, since the tag is appended rather than
  encrypted; a real receiver should strip a trailing 10-byte tag if suite 7 is
  confirmed on its host.

One correction to the probe, found when its RTCP check was ported: it masked the
second byte to seven bits before testing for 200–207, so the Mac's once-a-second
RTCP packets (type 200/201, read as RTP payload type 72/73) were decrypted and
written out as frames — the two "concealed" units the decoder reports on the
captured file are those. RTCP is told apart by the whole byte.

**Decoding AAC-ELD is the catch, and it is forced — the transmitter's codec is
decoupled from the negotiation.** AAC-ELD (MPEG-4 object type 39) is decodable by
neither FFmpeg's native `aac` decoder nor any browser's WebCodecs `AudioDecoder`, so
the stream cannot pass through and the gateway must decode. Apple's own
**AudioToolbox** (`aac_at`) decodes it, but only when the gateway runs on macOS, so
the portable answer is Fraunhofer's own decoder (licence not OSI-approved). The
AudioSpecificConfig it wants is `F8 E6 50 00` — object type 39, 48 kHz,
stereo, 480-sample frames, no SBR, no resilience tools — which decoded 375 of the
377 captured units cleanly (the other two were RTCP, above); 512-sample frames
concealed most of the stream and every other flag combination was refused.

**The decoder is Fraunhofer's Rust one, not its C one.** Android 17 ships "FDK2
AAC", a pure-Rust port of the fdk-aac decoder, in AOSP at
[`platform/external/aac`, `rust/`](https://android.googlesource.com/platform/external/aac/+/refs/tags/android-17.0.0_r1/rust)
(tag `android-17.0.0_r1`, the newest that repository carries at the time 0.0.241
is built). It is a Cargo crate named `aac` with no C code and no native build,
only ordinary crates.io dependencies, and it builds and runs on Linux as well as
Android. It is
decoder-only upstream — there is no encoder in it to leave unused. It takes the
stream in the shape this wire delivers:
`AacDecoderInstance::new()`, `config_raw(F8 E6 50 00)`, then
`fill` and `decode` per access unit, returning interleaved `f32` normalised to ±1
rather than `i16`, so `src/aac_eld.rs` multiplies by 32768 on the way out.
Symphonia is not an option: its AAC decoder, 0.6.1 included, decodes AAC-LC only
and refuses any other object type.

Two properties of it shape the code around it. Its `fill` reports the bytes it did
*not* take, where fdk-aac's reported the bytes it did, so the "whole unit consumed"
check reads `== 0`. And a decode *error* in the 0x4000 range is the concealment
case — output buffer valid, frame synthesised — which is the `is_decode_error()`
branch in `EldDecoder::decode`, not a failure.

It was measured against genuine Apple AAC-ELD without a Screen Sharing session.
`afconvert` on the Mac encodes through AudioToolbox:
`afconvert -f m4af -d "aace@48000#480" -b 320000 src.wav out.m4a` writes AAC-ELD
whose AudioSpecificConfig is the stream's `F8 E6 50 00`, with 480-frame packets and
240 samples of priming. On 10 s of stereo tones (1001 packets, macOS 26.6.2), both
decoders decoded every packet without error. They agreed with each other at 86 dB
SNR (within 3 LSB, because the Rust port is floating-point and fdk-aac is
fixed-point), and each reproduced the source at 40 dB SNR once aligned by the
240-sample priming. The Rust decoder took about 31 µs per 10 ms frame in a release
build. Fed 200 000 rubbish units — real ones with bits flipped, and pure noise —
in a build with overflow and debug assertions on, it never panicked: it concealed,
refused, and recovered on the next valid unit. That matters more here than it
looks, because the gateway builds with `panic = "abort"`, so a panic inside the
decoder would end the process rather than the stream.

Its instance holds `Rc`s, so it is not `Send` and cannot live in the future
`tokio::spawn` takes. `src/vnc_apple_audio.rs` gives it a thread of its own behind
a bounded channel: the socket task decrypts and sends one access unit per message,
the thread decodes and hands finished waves to the bridge, and dropping the sender
— which aborting the socket task does — ends it.

Choosing it did not change why the feature is gated: its licence is the same
"Fraunhofer FDK AAC Codec Library for Android" text, not OSI-approved and with no
patent grant. What it removed is the C side — the prebuilt static archive and its
`-sys` crate. It is not on crates.io, so it is a git dependency on
[a copy of that AOSP subtree](https://github.com/andrewtheguy/fdk-aac-rust) cut
down to what this stream needs: ER AAC ELD in mono or stereo without SBR, from raw
access units, and nothing else — every other object type, transport and
post-processing stage is gone, and the copy refuses a config that asks for one.
What is left decodes to the same samples as the unmodified decoder, which that
repository's tests pin, and being modified it has to call itself a "Third-Party
Modified Version of the Fraunhofer FDK AAC Codec Library for Android", which it
does.

That the codec cannot be moved off AAC-ELD is now **proven, not assumed.** Offering
a codec set that excludes AAC-ELD does not change the stream. Building a
round-trip-verified offer whose only audio codecs are AMR-NB (`{f1=1, f2=299}`) and
EVS (`{f1=4, f2=6500}`) — AAC-ELD (`{f1=16, f2=4100}`) removed cleanly at the
protobuf level, not byte-hacked — the server:

- **agreed to it**: its message-2 answer echoed back exactly `{1,299}` and `{4,6500}`
  and carried **no** AAC-ELD entry — the negotiation succeeded on AMR-NB/EVS; yet
- **streamed AAC-ELD anyway**: `pt=101`, RTCP timestamp stepping 480 samples/packet
  (48 kHz, 10 ms), ~370–410-byte stereo payloads, the `0x89ffffff` near-silence
  prefix, and a frame-1 header (`00683400…`) byte-identical to the all-AAC-ELD run.

So the `0x1c` media negotiation reconciles a codec *list* that the actual
`RemoteDesktopSystemAudio` transmitter then ignores: it encodes the system-audio
stream group's hardwired default (`defaultPayloadConfigurationsForStreamGroupID:`
maps that group to codec type `16`, AAC-ELD 48 kHz, unconditionally — there is no
Opus branch in it). The viewer's offer is a client-side input; the encoder is a
server-side setting it does not reach. **Opus is in AVConference's library (both
`_RegisterOpusEncoder` and `_RegisterOpusDecoder` exist) but not in this agent's
transmit path, and no offer can put it there.** The AAC-ELD decoder dependency is
therefore a proven necessity, not a worst case.

The server-side path was traced to the source in `ScreensharingAgent`
(`SSUDPSender`, the process that actually runs the sender). Its
`sendToRemoteAddress:…` builds the transmit config as
`audioConfig = [audioAnswerNegotiator generateMediaStreamConfigurationWithError:]`
and hands it to `createAVCAudioStreamWithRemoteAddress:connectedSocket:audioConfig:…`,
which does `[AVCAudioStream initWithNetworkSockets:options:error:]` + `configure:error:`.
The offer the viewer sent only ever reaches the *answer* the negotiator echoes back;
the `audioConfig` that configures the encoder comes from
`generateMediaStreamConfiguration`, which builds the stream from the audio stream
group's defaults (`VCMediaNegotiationBlobV2StreamGroupStream defaultsForStreamGroupID:`
→ `defaultPayloadConfigurationsForStreamGroupID:` → codec type `16`). That default
function is a plain switch on the stream-group FourCC with no preference, plist, or
environment read, so there is no server-side setting to flip either — the codec is
fixed by which stream group the agent opened, and the agent opens the system-audio
group.

### The negotiation codec set

Recovered by disassembling `AVConference` from the macOS 26.6.2 arm64e dyld shared
cache; the mapping is authoritative, the naming is from the routing functions and
symbols, not a spec. The `VCMediaNegotiationBlobV2` codec entries carry a
*negotiation codec type*;
`+[VCMediaNegotiationBlobV2StreamGroupPayload negotiationCodecTypeWithCodecType:]`
maps it to an internal codec type, and `isNegotiationCodecTypeAudio:` (mask `0x11b8`)
marks which are audio:

| neg type | internal | codec | audio? |
|---|---|---|---|
| 1 | 100 | (non-audio / screen) | no |
| 2 | 102 | (non-audio / screen) | no |
| 3 | 12 | EVS-family | **yes** |
| 4 | 11 | **AAC-ELD (SBR)** | **yes** |
| 5 | 16 | **AAC-ELD (non-SBR, 48 kHz stereo)** | **yes** |
| 6 | 300 | (non-audio) | no |
| 7 | 8 | audio (Opus / Comfort-Noise family) | **yes** |
| 8 | 4 | **EVS** | **yes** |
| 9 | 9 | (non-audio) | no |
| 10 | 301 | (non-audio) | no |
| 12 | 20 | audio (Opus / Comfort-Noise family) | **yes** |

So the audio family AVConference can negotiate is **AMR-NB, AMR-WB, EVS, AAC-ELD
(SBR and non-SBR), Comfort Noise, and Opus** — the FaceTime/telephony set.
`+[VCPayloadUtils bitrateForCodecType:mode:]` routes internal `1→`AMR-NB, `2→`AMR-WB,
`3/4/17→`EVS, `11→`AAC-ELD-SBR, `16→`AAC-ELD; the rate-mode mask `0x2001e` marks
`{1,2,3,4,17}` (AMR/EVS) as bitrate-adaptive. **Opus is definitely present**
(`codecConfigForOpusWithStreamConfig:`, `createSupportedBitratesForOpus`,
`opusSamplesPerFrameForSampleRate:blockSize:`, `opusRestrictedLowDelayEnabled`,
`isOpus4Channel48KhzPayload:`), but its exact internal id among the unpinned audio
slots (`8`, `12`, `20`) was not isolated. **Codec type 16 = AAC-ELD** is what the HP
screen-sharing agent (`RemoteDesktopSystemAudio`) actually configured and streamed
in every capture. This whole set is what AVConference can *negotiate*; it is **not**
what this agent will *transmit* — as the AMR-NB/EVS-only test above proved, the
transmitter emits AAC-ELD whatever the negotiation agrees, so the negotiable set is
of no help in escaping the AAC-ELD decoder.

## Still unknown

- Apple's still-image codecs `0x3ea` and `0x3f3`; the document leaves the first's
  rectangle body and the second's command-code table unresolved, and neither was
  advertised here, so nothing was learned.
- The media stream's **HEVC screen video** leg (`0x1c` video1/video2, SRTP). Only
  the audio leg was decoded — see "The media stream" above; the video offer had to
  be sent for audio to start, but its picture was never received or decoded.
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
