# Apple RFB 003.889, as measured

How a Mac's Screen Sharing behaves on the wire, as far as remotex depends on it.
None of this is documented by Apple. It was measured against macOS 26.5–26.6
Apple Virtualization guests between July and September 2026, and read from the
binaries those guests ship where the wire could not show a server's rules. A
macOS update is free to invalidate any of it.

This document states behaviour and the rules remotex follows because of it. The
evidence behind it is archived outside the repository, in
`apple-screensharing-audit-2026-09-23_2`:
- function-level traces of Apple's viewer, `screensharingd` and
  `ScreensharingAgent`;
- captures, daemon logs and probe scripts;
- a copy of this document's earlier, fully detailed revision.

The implementation is `src/vnc_record.rs` (the 003.889 record layer),
`src/vnc_apple.rs` (Apple's messages and encodings), `src/vnc_apple_media.rs`
(High Performance's media stream), `src/aac_eld.rs` (its sound's decoder) and
the two Apple paths in `src/vnc.rs`.

"High Performance" below is Apple's mode: a virtual display and the media stream.
Both modes speak RFB 003.889, Apple's own revision, inside its encrypted record
layer.

| Subtype | Mode | Picture | Sound |
|---|---|---|---|
| `ard` | Standard, the physical displays | ZRLE | none; the Mac's own output is left alone |
| `ard-high-performance` | High Performance, one virtual display | HEVC over the media stream, ZRLE until it is up | AAC-ELD over the media stream |

`ard-high-performance` is High Performance as Apple's viewer has it, and needs a
gateway built with the `apple-hp-media` feature. Remotex does not offer a
virtual display without the media stream: Apple's viewer never offers that
combination.

## Summary

| | |
|---|---|
| Two subtypes | Both speak RFB 003.889 with an encrypted record layer, as Apple's viewer answers every Mac. `subtype = "ard"` is Standard mode, sharing the Mac's physical displays at a fixed size. `ard-high-performance` is High Performance mode, sharing one virtual display the Mac creates at the size the client asks for. |
| Confirmed | Type-30 authentication, the record layer and its initial rekey, zlib and ZRLE, the cursor cache, the display layout and the metadata framing. |
| Corrected | Several published reverse-engineered descriptions are wrong on points remotex depends on: the layout's length and display count, `ViewerInfo`'s body, the virtual display's maximum size, and `AutoFrameBufferUpdate`. So are the pointer buttons on this revision and the wheel. Each is covered below. |
| Density | A virtual display is asked for at 1x or 2x only; a fractional ratio is not rounded and produces a zoomed desktop. Standard mode is scaled by the Mac to the browser's density, and a mixed-density All Displays view is composed in the browser, as Apple's viewer does. |
| Picture and sound | `ard` is ZRLE throughout, and carries no sound: Standard mode never touches the Mac's sound output. `ard-high-performance` takes both from the media stream, as Apple's viewer does — HEVC and AAC-ELD over SRTP — and its picture from ZRLE until the stream is up and across display changes. |
| Not implemented | Apple's controls for two virtual displays and fixed presets; its viewer's rate feedback on the media stream; authentication types other than 30. |

## Remote Management access

Rule out the Mac's Remote Management permissions before treating an
authentication failure as a protocol fault. When the account lacks permission,
the type-30 exchange completes before the Mac refuses, exactly as it does for a
wrong password. Both Apple subtypes then report
`VNC authentication failed: <the Mac's reason>`, which does not tell the two
causes apart.

Remote Management's default **All users** setting rejects valid account
credentials. Add the account to the per-user access list and grant at least
**Observe** and **Control** (the Mac records this as `ARD_AllLocalUsers = 0` in
`/Library/Preferences/com.apple.RemoteManagement`). Configure it under System
Settings → General → Sharing → Remote Management (ⓘ) → the account, or over SSH:

```sh
sudo /System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/Resources/kickstart \
  -configure -access -on -users <account> -privs -all -restart -agent
```

**VNC viewers may control screen with password** is for RFB security type 2.
Remotex authenticates with type 30 and the account's own password, so it does not
need that setting.

Apple's viewer also knows private security types 31–36: Diffie-Hellman variants,
RSA, a preauthorized connection, Kerberos and SRP. Only type 30 has been
exercised, and only type 30 supplies the key the record layer starts from, so
remotex offers nothing else.

## The two modes in Apple's viewer

Apple's viewer's connection sheet offers **Standard**, with a choice of Adaptive
or Full quality, or **High Performance**, with one or two virtual displays. The
choice is made after ServerInit; the handshake before it is the same in both
modes (see [Connecting](#connecting)).

| | Standard | High Performance |
|---|---|---|
| Encodings | Adaptive: Apple's private `0x3f3` and `0x3ea`, then zlib and ZRLE. Full: zlib, then ZRLE. | The media stream (1010), then as Adaptive. |
| Displays | The physical ones, selected with `SetDisplay`, scaled with `SetServerScaling`. | One or two virtual displays from `SetDisplayConfiguration`, 60 Hz unless a preference says otherwise. |
| Quality menu | Adaptive or Full. | Disabled while the media stream runs. |
| Refused beside it | A virtual display, dynamic resolution, HDR. | No virtual display, or a screen other than the virtual ones. |

- **A Mac without High Performance.** Apple's viewer offers the mode only when the
  Mac's ServerInit lists `SetDisplayConfiguration` and a feature flag of the
  viewer's own is on. Otherwise it asks whether to continue, then connects in
  Standard mode with no virtual display. It never runs High Performance on the
  physical displays. `ard-high-performance` has no one to ask, so it refuses the
  session and names `ard`.
- **A media-stream failure.** An error from the Mac (message 3) makes Apple's
  viewer show an alert and close the session. So does a stream that has not
  started after three RTCP timeouts on a leg, and one that has run and then
  reaches 16, and the media stack sets that timeout to 3 s for screen sharing.
  It has no fallback to RFB pixels, and `ard-high-performance` has none either (see
  [Liveness](#the-stream)).

Remotex matches the split, on the same handshake: `ard` shares the physical
displays, refuses `resize` and never creates a virtual display, and
`ard-high-performance` creates exactly one, the "1 Virtual Display" choice, and
never selects a physical screen or sends `SetServerScaling`. It departs in two
places:
- **Encryption.** Apple's viewer asks for the record layer only when its
  `encryptionLevel` preference is 2. The default is 0, which leaves the whole
  session in cleartext after authentication, keystrokes and the media stream's
  keys included. Remotex always asks, in both modes.
- **Standard's picture.** `ard` asks for ZRLE alone, where Full quality asks for
  zlib first and Adaptive first for the private codecs remotex cannot decode.

## Connecting

Both modes connect the same way until the record layer is up.

1. **Version.** `RFB 003.889`, as Apple's viewer answers every Mac. It sends
   `003.003` to a server that is not a Mac.
2. **Type 30.** A Diffie-Hellman exchange. `MD5(shared secret)` is the AES-128 key
   that encrypts the 128-byte credential block (username at 0, password at 64) in
   **ECB** mode, not the CBC a published description gives. It is also the first
   key the record layer's rekey is wrapped under.
3. **ClientInit** `0x81`: `0x80` asks for Apple's extended ServerInit, and `0x40`,
   never set, would ask for a session-select exchange remotex does not implement.
4. **ServerInit**, extended (see below). High Performance ends here, before sending
   anything, when the Mac does not list `SetDisplayConfiguration`.
5. **A cleartext prelude:** `ViewerInfo`, `SetMode(control)`, and
   `AutoPasteboard(start)` when clipboard is on, then `SetEncryption` commands 1
   and 2. The Mac answers with the rekey, and everything after it travels in
   records.
6. **The mode.** High Performance sends `SetDisplayConfiguration`, both modes send
   `SetPixelFormat` and the same `SetEncodings`, and High Performance then arms
   `AutoFrameBufferUpdate`. Standard arms it when the first layout names the
   screen being sent.

### ServerInit's name field is not a name

In the extended ServerInit, the "name" is 22 bytes of structure and then the
UTF-8 name: a zero `u16`, a `u32` of server flags, a 16-byte capability bitmap,
the name. Read as a name, it prints as mojibake.

The bitmap lists the client message types the Mac accepts, one bit each, most
significant bit first: type `t` is bit `7 − t % 8` of byte `t / 8`. The measured
Mac sends `bf f6 e7 2f ec` and zeros, which lists `SetDisplayConfiguration`
(`0x1d`), the media stream's `0x1c`, `SetEncryption` (`0x12`) and `ViewerInfo`
(`0x21`) among others. Apple's viewer offers High Performance only to a Mac that
lists `0x1d`, and `ard-high-performance` refuses the session on one that does not
(see [The two modes in Apple's viewer](#the-two-modes-in-apples-viewer)).

The flags:

| Bit | Meaning |
|---|---|
| `0x01` | Observe only; control is refused. |
| `0x02` | The user holds the control privilege. |
| `0x04` | A session-select block follows (only when the client set `0x40`). |
| `0x08` | Screen capture is not permitted. |
| `0x10` | Set in the extended ServerInit. |
| bits 5+ | The maximum number of virtual displays (2). |

### Which encodings make the Mac report its displays

The Mac reports its screens according to which of two encodings the client's
`SetEncodings` lists. Order and duplicates make no difference, and every
`SetEncodings` that lists `DisplayInfo` produces another report.

| `SetEncodings` lists | the Mac sends |
|---|---|
| `AppleDisplayLayout` (`0x451`) and `DisplayInfo` (`0x44d`) | the layout |
| `DisplayInfo` without `AppleDisplayLayout` | the older `DisplayInfo` |
| neither | nothing about its displays |

`vnc_apple::ENCODINGS` asks for ZRLE from the start, and for no zlib, along with
the layout.

Advertising is a promise: every advertised encoding must be decodable, or at least
steppable. `CursorPos` (`0x44c`) has no payload. `DisplayInfo` is 10 bytes of
header, then `0x1c` bytes per screen. The other metadata encodings each start with
a `u16` giving how much follows.

## The record layer

Every message after the rekey, in both directions, is a record:

```text
u16 ciphertext_len
AES-128-CBC( u16 body_len || body || filler || 20-byte integrity )
```

- **Chaining.** One CBC context per direction for the whole session: a record's
  last ciphertext block is the next record's IV.
- **Filler.** `(−(2 + body_len + 20)) mod 16` bytes; zero filler is accepted.
- **Integrity.** `SHA1(u32_be(seq) || the plaintext before it)`, with an
  independent sequence counter per direction starting at 0.

A server message can span records: a full-screen zlib rectangle is about 400 KB
against a record ceiling of 65,520 bytes, so records are reassembled by
concatenation, never read one message per record.

**The rekey** arrives as a one-rectangle framebuffer update with encoding `0x44f`
and zero geometry. Its body is a `u32` generation, then a wrapped key and a
wrapped IV, each one AES-128 block decrypted under the wrap key. The Mac rotates
keys only when the viewer asks with `SetEncryption` command 1, and it switches
both of its ciphers the moment it sends a rekey. Remotex asks once, during setup.
It closes the session on any later rekey rather than follow it, because records
it had already framed under the old key would fail the Mac's check.

The Mac may send a `MiscStatus` in the cleartext window between `SetEncryption`
and the rekey, notably after a server restart with stale clipboard state; the
client must step over it.

**ZRLE** (`0x10`) is standard RFB: one deflate stream for the life of the
connection, each rectangle a `u32` length and a chunk of it holding 64×64 tiles,
each raw, run-length encoded, palettised or both, with three-byte CPIXELs in the
pixel format `SetPixelFormat` asks for. The Mac encodes it whenever it is the first
of zlib, ZRLE and its own codecs listed.

**zlib** (`0x06`), not listed, is the same stream holding raw
`w × h × 4` pixels. On a static desktop it measured roughly 50:1, in either mode.

**The cursor cache** (`0x450`) stores a shape when its compressed length is nonzero
and selects a stored one when it is zero. A shape is a `w·h·4` BGRA pixmap followed
by a separate `w·h` alpha plane. Each stored shape is its own zlib stream, so a bad
one can be skipped without disturbing the next or the framebuffer's stream.

## Displays

### The display layout

`AppleDisplayLayout` (`0x451`) is how the Mac says which screens it has, which one
it is sending, and at what size. It arrives whenever that changes — including at
every login, lock and user switch — as a one-rectangle framebuffer update.

**Arming.** A layout drops the Mac's update arming. The client must re-send
`AutoFrameBufferUpdate` after each one, or the pointer silently stops updating
while the desktop keeps painting.

**The layout is authoritative.** Remotex moves a display selection's checkmark
only when a layout confirms it.

### A layout's length counts what follows it

The payload starts with a `u16` counting the bytes after itself:
`0x14 + displays × 0x38`. A reader that counts the prefix in its own length
starts every field two bytes out and leaves four bytes on the stream, where they
parse as a phantom empty update.

The header, after that prefix:

| Offset | Field |
|---|---|
| `+0x00` | `u16` version, 5 |
| `+0x02` | `u16` × 2: the whole desktop's size in points |
| `+0x06` | `u16` × 2: the framebuffer's size in pixels, which changes with the selection |
| `+0x0a` | `u32`: the screen being sent, or `0xffffffff` for all of them |
| `+0x0e` | `u32`: session state (on console, obscured, locked, login pending) |
| `+0x12` | `u16`: display count, 1–25 |

### A display record, as sent

Each record is `0x38` bytes:

| Offset | Field |
|---|---|
| `+0x00` | `f64` BE: this screen's native density, 1.0 or 2.0; 0.0 if the Mac could not look its mode up |
| `+0x08` | `f64` BE: the server-side scale applied to it, 1.0 unless the client asked for scaling |
| `+0x10` | `u32`: the display id |
| `+0x14` | logical bounds in points, as `(top, left, bottom, right)` `u16`s |
| `+0x1c` | backing bounds in framebuffer pixels, the same way |
| `+0x24` | `u32` flags: bit 0 main, bit 1 in a mirror set, bit 2 dynamic virtual display |
| `+0x28` | 16-byte pixel format |

Bounds are edges, not an origin and size: a size is a difference of edges.
Members of a mirror set share an origin, and remotex offers the first. When the
density field is 0.0, remotex derives the density from the two rects rather than
dropping the screen. For High Performance's single record, dropping it would end
the session.

### Picking a physical screen in Standard mode

`SetDisplay` (`0x0d`) selects one physical screen, or all of them combined. The
next layout names the screen being sent, and the framebuffer becomes that
screen's own pixels or the union of all of them. On the measured Mac these were:

| Sent | The layout's current screen | The framebuffer |
|---|---|---|
| all displays | `0xffffffff` | 4480×1800, the union |
| display 4 (a 2x screen) | `4` | 3200×1800 |
| display 1 (a 1x screen) | `1` | 1280×800 |

### Server-side scaling in Standard mode

Standard shares physical displays at a fixed size: it requires `resize = false`
and never sends a viewport size. It does honour a scale.

**`SetServerScaling`** is ten bytes: `0x08`, a reserved zero, then a factor in
(0, 1] as a big-endian `f64`. The Mac renders the framebuffer at that factor
before encoding it, and the answering layout reports the factor in each record's
second `f64`.

**What remotex asks for.** It asks for `min(1, browser density / screen density)`,
as Apple's viewer does. A 1440×900 Retina screen (2880×1800 native) then reaches a
1x browser as 1440×900 at effective density 1, and a 2x browser at native
resolution. The Mac cannot enlarge, so a 1x screen reaches a 2x browser at 1x.

**Pointer positions.** The Mac still reads pointer events in the *unscaled*
framebuffer's pixels. So remotex divides each browser position by the factor in
force; without that, the pointer lands at a fraction of its distance from the
origin.

**When remotex asks.** A browser density change or a new selection can send a new
factor. Only an answering layout confirms it, and a request left unanswered for
ten seconds is given up. The Mac handles the request at once, but its answer can
take seconds to arrive behind a display switch.

### All Displays over mixed densities

No single factor renders a 1x screen beside a 2x one. Apple's viewer does not try.
In All Displays over mixed densities it never sends `SetServerScaling`; it takes
the native combined framebuffer and draws each screen's region at that screen's
size in points:
- at medium interpolation;
- over a dark grey background where the screens leave gaps;
- with pointer input over a gap dropped.

Its menu names the view by the points the screens span ("Both Displays:
2720 × 900"). Its Separate Windows option is the same single connection, with
each window drawing one screen out of the same framebuffer.

"Mixed" is what Apple's viewer treats as mixed: at least one screen at 1x and at
least one that is not. Screens that are all HiDPI, whatever their densities, are not
mixed.

Remotex matches it. For a mixed combined layout, the gateway asks for factor 1.0
and sends a `ServerMsg::Mosaic`, each screen's rectangle in framebuffer pixels
and in points, ahead of the `Resize` (flagged so the browser adopts it with that
framebuffer, not over the one on screen). The browser then:
- composes the regions at its own density, at medium smoothing, over the same
  grey;
- maps pointer positions back through the regions, re-mapping the position
  before each press and wheel and dropping them in gaps (a button release still
  goes, so a drag cannot leave a button held).

This is the one place the browser rescales remote pixels (see AGENTS.md).

At factor 1.0 the combined framebuffer is the screens' native pixels side by side,
and it is often past what a video stream encodes: a 2x screen beside a 1x one
measured 5376×2287. Standard never resizes (`resize = false`), so the gateway
cannot ask for less. Instead it passes the Mac's own rectangles through: each ZRLE
rectangle of a `FramebufferUpdate`, decoded, goes to the browser whole as one PNG
tile at the Mac's place and size, and the mosaic composes the framebuffer they are
drawn into. Nothing about the RFB side changes — the Mac sends ZRLE rectangles
either way. Going back to one screen returns the session to video at the next
layout. See
[tiles past the ceiling](architecture.md#tiles-past-the-ceiling).

### The High Performance virtual display

High Performance hides the Mac's physical screens and moves every window to a
virtual display. Remotex creates one, at the pinned `width`/`height` or else at
the full size of the browser's screen and at that screen's density, as Apple's
client does. The window layout macOS produces depends on that opening size:
windows squeezed onto a small opening display do not spread out again when it
grows. With `resize = true`, each viewport report asks for a new size, and the
next layout confirms it.

`SetDisplayConfiguration` (`0x1d`) carries one display descriptor and one mode:

| Descriptor field | Value remotex sends |
|---|---|
| name | 120 bytes |
| display flags | 1: dynamic resolution (bit 1 would supply a custom refresh rate) |
| display type | 4, virtual |
| physical size | millimetres, as big-endian `f32` |
| maximum backing size | 3840×2160, a fixed ceiling |
| rotations | 7, Apple's captured value; its bits are private |
| mode count | 1 |

The mode itself holds:
- a backing size;
- a logical ("scaled") size;
- a refresh rate: 30 Hz (see the media stream's rate below);
- flags (bit 0 HDR).

A backing size twice the logical one makes a 2x display.

**Rules that follow from the measurements:**
- **The maximum is a ceiling.** Putting the current mode there made the Mac refuse
  anything larger. With 3840×2160 it accepted every size asked for, including
  bursts of them and sizes below the 800×600 Apple's own UI allows.
- **Ask for 1x or 2x only.** The Mac does not round a fractional ratio. 1.5x and
  1.25x modes produced 2x displays of fewer points, with zoomed text and a Dock
  shrunk to fit.
- **The first descriptor is always dynamic,** even with `resize = false`, so a
  reconnect re-enables the Mac's Dynamic resolution setting. `resize` controls only
  whether remotex acts on later viewport reports.
- **The display outlives its session briefly.** A reconnect within a few seconds
  finds it still there with the same id; after about 45 seconds the Mac is back on
  its physical display. The new session's own layout arrives either way.

### Resizing a High Performance display, as measured

Three behaviours of the Mac shape how remotex resizes:

- **A display change stops the Mac's media stream.** The Mac restarts nothing until
  it is offered again, so the picture is ZRLE's until the new display's stream
  delivers — see [Display changes](#display-changes).
- **A read racing a shrink crashes the Mac's capture agent.** Serving a pixel read
  sized for the old display after the display shrank crashes `ScreensharingAgent`.
  The session then loses its virtual display, and often its connection. The update
  arming counts as such a read. So remotex:
  - sends a change only at the end of an update, with no full-size request
    outstanding;
  - first re-arms updates for the single pixel at the origin, which every mode
    has;
  - polls that pixel incrementally until the answering layout;
  - never has two changes outstanding.
- **The Mac reads nothing while it writes an update.** It holds the connection's
  lock while it compresses and sends a rectangle. A client that drains a
  multi-megabyte repaint slowly therefore leaves its own messages unread until the
  repaint is through. A debug-build gateway saw 20–30 s stalls; a release build
  sees changes answered in about 2.5 s. Updates the Mac pushes unasked hold the
  same lock, which is why remotex paces them (see
  [Other messages](#other-messages)).

## Input

### Apple's revision reads the pointer mask as CGMouseButton numbers

RFB's mask is bit 1 left, bit 2 middle, bit 3 right. The Mac swaps bits 2 and 3
for every protocol version except 3.888 and 3.889, and its agent reads the mask as
macOS button numbers (left, right, center). So on 003.889 a by-the-book
right-click arrives as a middle-click, which macOS does nothing visible with, in
either mode. `Buttons` in `src/vnc.rs` swaps the two bits for both Apple subtypes.

### A Mac scrolls only on a lone wheel bit

The Mac scrolls only for a mask of exactly `0x08` (up) or `0x10` (down). Any other
combination is posted as buttons: a wheel bit with a button held becomes Back or
Forward, and the horizontal bits `0x20`/`0x40` become clicks on buttons 5 and 6.
Each pulse scrolls only about two pixels. Remotex sends each vertical pulse
alone, sends no horizontal ones, and sends pulses in proportion to the scroll
distance (`src/vnc.rs`).

### Keys

- **Modifiers follow the keysym.** An uppercase letter brings Shift with it, and a
  held Shift is stripped from a lowercase one. A shortcut under Caps Lock must
  therefore be sent as the lowercase keysym, or Command-Z arrives as
  Command-Shift-Z.
- **Keys with no mapping.** Insert, Pause, Scroll Lock, Print and Menu have none on
  the Mac, and Num Lock arrives as Keypad Clear.
- **Option.** Option is stripped from ordinary keys unless Command is also held.

### Double-click is chained by the Mac, at a login-time threshold

An RFB pointer event carries no click count, so the Mac decides which presses
chain into a double-click. They chain when they land on the same spot within its
double-click interval. That interval is read once at login: changing the
preference has no effect on a live session until logout or reboot. A Mac that
double-clicks in Apple's client but not through remotex has a stale or very short
threshold. Set `defaults write -g com.apple.mouse.doubleClickThreshold -float 0.5`
on the Mac and reboot. Remotex forwards clicks as they happened, with no
compensation.

## Other messages

**`AutoFrameBufferUpdate` (`0x09`) makes the Mac push while the screen changes.**
Its body is a `u16` version (1), a `u32` interval in microseconds and the armed
rectangle.
- **A still screen gets nothing unrequested,** armed or not, so a client that stops
  polling paints one frame and freezes. Remotex keeps polling.
- **A changing screen is pushed as fast as the Mac captures it** when the interval
  is 0. A YouTube video playing on a 1920×1080 virtual display drew 60–90 updates
  a second, 15–33 MB/s of zlib, for two requests. Unarmed, the same screen drew
  nothing after the update asked for.
- **The interval paces the pushes.** At 1,000,000 the Mac pushed about one update
  a second, which is what remotex arms with.

Unpaced pushes cost a client its input. The Mac
[reads nothing while it writes an update](#resizing-a-high-performance-display-as-measured),
and pushed updates follow one another for as long as the screen changes. A client
that drains the connection more slowly than the Mac captures — a busy gateway, a
slow link — has its clicks, keys and display changes left unread for the length of
the video. On a Mac playing one, input went unread for 35–104 s at a time and then
arrived as hundreds of queued events in one second. Apple's viewer never meets
this: in High Performance mode it takes the picture from the media stream
(encoding `0x3f2`), and the daemon's framebuffer sender skips such a viewer.
Remotex does too once the stream is up, and then arms and polls one pixel —
see [RFB while the stream runs](#rfb-while-the-stream-runs).

Arming the full framebuffer at setup and after every layout is still required,
because it keeps cursor updates alive across logins and locks. Its rectangle is
not a flow-control knob: changing it mid-session corrupts later updates, except
for the one-pixel arming around a High Performance resize and while the media
stream carries the picture.

**`ViewerInfo` (`0x21`) is 66 bytes of numbers.** The published description
implies version strings; the body is two numeric version triples:

| Field | Value remotex sends |
|---|---|
| application class | 1 |
| application id | 2 |
| application version | 6.1.0 |
| OS version | 26.6.2 |
| capability bitmap | 32 bytes: the server message types the viewer handles |

A mis-sized body makes the Mac swallow the next message and hang silently. The
capability bitmap gates whether the Mac sends `MiscStatus` at all.

**The pasteboard.** Change notifications need `ViewerInfo`, `SetMode(control)`
and `AutoPasteboard(start)`, in that order. Both modes send them in the cleartext
prelude, and High Performance repeats `AutoPasteboard(start)` after the virtual
display's layout. The Mac then signals with `MiscStatus`:
- command 2: its pasteboard changed;
- command 3: it needs data for a promised flavor.

Contents travel as a zlib archive (level 9, one sync flush, capped at 100 MB) of
every flavor of every item. A short text selection can therefore arrive inside
megabytes of other flavors. Remotex streams the archive, keeps only the text, and
sends empty text as an item with no flavors, which clears the Mac's pasteboard.

**Polling pauses behind a fetch.** Framebuffer and pasteboard replies share one
ordered stream. While a pasteboard fetch is pending, remotex pauses incremental
polling, so the fetch is not stuck behind a stream of updates.

**Metadata arrives only as rectangles.** The layout, keyboard source, vendor
keysyms and device information are each a one-rectangle framebuffer update.
Apple's viewer closes the connection on a server message type it does not know,
so a reader that falls out of step sees "messages" that are really fragments of
these.

### The numbers, in both forms

Apple writes its encodings in hex, while the wire and RFB's registry use decimal,
so remotex logs an unexpected encoding as `1105 (0x451)`.

| Encoding | Hex | Decimal | |
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
| media stream | `0x3f2` | 1010 | in a second `SetEncodings` |
| ZRLE | `0x10` | 16 | standard RFB |
| zlib | `0x06` | 6 | standard RFB, not advertised |
| Raw | `0x00` | 0 | standard RFB |
| `DesktopSize` | — | −223 | pseudo-encoding |
| `LastRect` | — | −224 | pseudo-encoding |

Message types: `MiscStatus` `0x14`, `AutoFrameBufferUpdate` `0x09`, `ViewerInfo`
`0x21`, `SetDisplayConfiguration` `0x1d`, `SetDisplay` `0x0d`, `SetServerScaling`
`0x08`, and the media-stream negotiation `0x1c`.

## The media stream: High Performance's picture and sound

In High Performance mode Apple's viewer takes neither its picture nor its sound
from RFB. RFB only negotiates a media stream: the viewer sends an offer, and
`ScreensharingAgent` then sends the screen and the system audio through
AVConference — the FaceTime media stack — as HEVC and AAC-ELD over UDP with SRTP,
straight to the viewer. Remotex does the same on an `ard-high-performance` target
(`src/vnc_apple_media.rs`). ZRLE carries the picture only until the stream
delivers and across display changes. A stream that fails ends the session, as it
ends Apple's viewer's: one the Mac refuses, one that brings no picture or no
sound, and one that stops (see [Liveness](#the-stream)).

Remotex decodes the picture and encodes it as VP9, unless the target sets
`hevc_passthrough` and the browser decodes the Mac's HEVC: then each access unit
goes to the browser as it came, described by the stream's own sequence parameter
set, and ZRLE's rectangles fill the gaps as VP9 encoded here. A PLI is its repaint. See
[Apple's HEVC, passed through](architecture.md#apples-hevc-passed-through).

The two decoders are the `apple-hp-media` Cargo feature, off by default and in
no release artifact: FFmpeg's HEVC decoder for the picture (libavcodec,
LGPL-2.1-or-later, linked statically) and Fraunhofer's AAC-ELD decoder for the
sound (a licence that is not OSI-approved). A build without the feature refuses `ard-high-performance` when
it reads the config. The wire half of the module — the offers, the replies,
SRTP and the depacketizer — is compiled and tested in every build.

### Negotiation

After the first layout the viewer sends a second `SetEncodings`, the opening list
with encoding 1010 (`0x3f2`) appended, then message `0x1c`
(`RFBMediaStreamServerConfiguration`, version 3):

```text
+0x00 u8   0x1c
+0x01 u8   pad
+0x02 u16  body length (everything after this field)
+0x04 u16  version = 3
+0x06 u32  flags
+0x0a u16  audio offer length
+0x0c u16  video1 offer length
+0x0e u16  video2 offer length
+0x10 u32  zero
+0x14 16B  session UUID
+0x24 46B  audio SRTP master key, viewer -> server
+0x52 46B  audio SRTP master key, server -> viewer
+0x80      audio offer, then the video1 keys (46B v->s, 46B s->v) and offer
```

The Mac answers with rectangles of encoding 1010, a `u16` size and then:

- **message 1**, a 36-byte body: `u16` type, `u16` version, `u32` flags, then a
  `u16` port and `u32` flags for audio at `+8`/`+10`, video 1 at `+14`/`+16`,
  and video 2 at `+20`/`+22`, followed by ten reserved zero bytes. Bit 0 enables
  a leg. Apple's viewer requires it on audio and video 1; remotex also requires
  video 2 off because it offered one display. The measured ports were always
  5900 and 5901, the RFB port and the next. The viewer receives on the same
  numbers.
- **message 2**, AVConference's answer: the common eight-byte header, `u16`
  lengths for the audio, video 1, and video 2 answer blobs, a zero `u32`, then
  those blobs. Remotex checks that their lengths describe the whole body and
  that video 2 is empty. The answer sometimes comes twice for one offer.
- **message 3**, a 16-byte error: the common header, then `u32` type and `u32`
  sub-code.

Each offer is a binary property list of four keys around a deflated
AVConference protobuf. Remotex rebuilds Apple's offers field by field and changes
three fields:

| Field | Apple's viewer | Remotex | Why |
|---|---|---|---|
| `0x1c` flags | 0 | `0x5` | Bit 2 makes the agent capture without the pointer (`send cursor with video 0`). Without it the pointer is drawn into every picture. Bit 0 is 60 fps, which the daemon sets anyway for a viewer older than version 2; it does not bound the picture rate, the virtual display's refresh does. |
| `tilesPerFrame` (video stream field 6) | 4 | 1 | Four tiles split a frame into strips of 256 rows. Each strip is coded as a separate picture of one bitstream, in its own sequence-number space with a DONL, and nothing in a packet names its strip. One tile is one picture of the whole display, without DONL. |
| bitrate entries (codec list, `f1 = 0`) | up to 100 Mbit/s | capped at 12 Mbit/s | The cap is the ceiling of the Mac's rate control, whose floor is 20 Mbit/s ([Rate control](#rate-control)). Below the floor the encoder runs at the cap: an animating lock screen came at about 7 Mbit/s under an 8 Mbit/s cap. |

**The picture and the sound go together.** A configuration with an empty audio
offer is refused (`unable to create audio config`, error type 2), and one with an
empty video offer (`unable to create video config`, the same type). While the
audio leg runs, the Mac mutes its own sound output: the daemon's log shows the
output device muted as the stream starts, and the Mac's speakers were measured
silent. So an `ard-high-performance` target always carries sound and takes no
`audio` key. The stream takes over the Mac's sound, AirPlay included. Standard
mode never touches the sound output, so a Mac there plays to its speakers or to
an AirPlay receiver outside remotex as usual; in a High Performance session it
plays nothing to one, which was confirmed on a physical Mac.

**One offer at a time.** A second `0x1c` sent while the first one's capture was
still starting left the capture failed (`didStart: 0 error: 32000`). When the
virtual display was deallocated at the end of that session, WindowServer aborted
in `WSSelectiveSharingUpdateDisplayStreamSurface` and logged the console user
out. Remotex therefore has one offer out at a time, and sends no display change
while an offer is unanswered. An offer left unanswered ends the session with the
other failures (see [Liveness](#the-stream)).

### The stream

- **RTP.** Payload type 100, with a one-word header extension under profile
  `0x9311` or `0x9301` holding the picture's packet count and a frame counter;
  remotex ignores it. The marker bit ends a picture. RFC 7798 packetization:
  single NAL units, aggregation packets, fragmentation units.
- **HEVC.** Range Extensions profile, 8-bit 4:4:4, full-range BT.709 matrix, sRGB
  transfer, Display P3 primaries, with wavefront parallel processing
  (`entropy_coding_sync_enabled_flag`) and no tiles. The prebuilt libavcodec
  (FFmpeg 9.0.2, configured down to the HEVC decoder) decodes it. On one
  core of an i5-8500T a 1600×1000 picture takes 14–23 ms, too slow for 60 a
  second. Remotex gives the decoder four slice threads, which decode a
  picture's rows in parallel and took 7–14 ms; frame threads would hold each
  picture back. A unit that fails to decode is an error, not a skipped picture,
  and brings a keyframe request.
- **Rate.** A picture goes out when the screen changes, at most once per refresh
  of the virtual display. Under a full-screen animation, a 60 Hz display sent
  about 57 pictures a second, with the `0x1c` 60 fps flag or without it, and the
  Mac logged `viewer set refreshRate 60` and `encode frame rate 60` either way.
  A 30 Hz mode sent 30.0, across resizes, and logged `viewer set refreshRate 30`.
  Remotex asks for 30 Hz, because the browser is sent 30 frames a second and
  every picture has to be decoded whether it is shown or not. A still screen
  sends none for as long as it stays still: 75 s without a picture on macvm,
  while the Mac's sender reports went on. The receiver's debug log reports the
  pictures a second every 10 seconds.
- **Receiving.** The Mac sends each picture as one burst at the link's speed. On
  macvm at 3200×2000, over 40 s of video with two display changes, a socket with
  Linux's default 208 KB receive buffer lost 9% of a passed stream's datagrams and
  56% of a decoded one's, keyframe fragments among them, until a display never had
  its first picture and the session ended. A receiver on a thread of its own,
  apart from the RFB connection's, still lost 16–23% and ended the same way, so
  the burst outruns the buffer however promptly it is read. Remotex asks for 4 MB
  on the video's socket, and with it granted the same runs lost none, the receiver
  sharing the engine's thread. Linux grants no more than `net.core.rmem_max`, and
  the log warns when it grants less.
- **SRTP.** AES-256 counter mode with an HMAC-SHA1-80 tag, keyed by RFC 3711 from
  the 46-byte masters in the offer. Received packets use the server-to-viewer
  key; this side's SRTCP uses viewer-to-server. The Mac's own reports are SRTCP
  under its key: a sender report on each leg about once a second, media or not,
  authenticated for liveness and never decrypted. An authentic packet no newer
  than one already received, a duplicate or a straggler, is dropped on both
  legs.
- **RTCP.** The viewer sends a receiver report on both legs every second. A PLI or
  FIR brings an IDR within about 30 ms. Remotex sends a PLI after a loss, when
  a stream starts without an IDR (the first packets can arrive before the socket
  is bound), and when the decoder falls eight pictures behind, which it warns
  about; for a passed stream, when the browser's link falls 15 behind and when
  the browser has to start over. It sends none of the rate reports described under
  [Rate control](#rate-control), because its cap sits below the range they act in.
- **Liveness.** Every offer owes its answer, its display's first picture and
  the first sound packet within 10 s, and the running stream an authentic
  packet, SRTP or SRTCP, on each leg every 48 s, 16 of Apple's 3-second
  timeouts. Apple's viewer times each leg from the last RTCP packet it
  received, not from pictures, which a still screen stops. The Mac's
  once-a-second reports keep both legs alive, and the sound leg also sends a
  packet every 10 ms whether or not anything plays. Past any of them, the
  session ends, as it does when the Mac refuses the offer (message 3)
  and when the receiver fails, on a socket error or a decoder, HEVC or AAC-ELD,
  that cannot start or stops. A display change stops the stream and owes nothing
  until its own offer, except the answer to an offer still out. When the Mac
  names its ports and nothing arrives within 5 s, the log names the port and the
  likely firewall or NAT.

### Rate control

The Mac's encoder follows a rate controller on the Mac that works from the
viewer's reports alone. It moves between a floor of 20 Mbit/s and a ceiling of
the offer's bitrate entries, capped at 60 Mbit/s. The floor and the 60 Mbit/s
ceiling are the Mac's own settings for screen sharing, and no offer field is known
to lower the floor. The daemon logs the controller's state every 5 s: target,
cap, measured bitrate, round-trip time, one-way delay and loss.

- **The report.** An RTCP APP packet named `RCTL` with a 20-byte payload, sent
  on the picture's leg as SRTCP. It must be the only packet in its datagram: the
  Mac rejects one inside a compound packet as a bad APP packet. The payload is
  big-endian:

  ```text
  +0  u8   0x85
  +1  u8   a millisecond figure / 20, meaning unknown; 0 is accepted
  +2  u16  4, the payload's length in words after this field
  +4  u16  echo: the last received picture packet's RTP timestamp >> 8
  +6  u32  zero
  +10 u16  milliseconds since that packet arrived
  +12 u16  the viewer's clock, in 1/1024 s
  +14 u16  one-way relative delay, seconds x 8192, at most 0xffff
  +16 u16  bursty loss (top 4 bits) | picture packets received mod 4096
  +18 u16  bandwidth estimate, kbit/s
  ```

- **The echo.** The Mac keeps a history of what it sent keyed by the picture
  leg's 24 kHz RTP timestamp shifted right by 8, about 94 entries a second, and
  takes the round-trip time from the echo and the hold time after it. An echo
  that matches nothing logs a missing send-history element and leaves the
  controller without a round-trip time or a measured bitrate. The received count
  is a running one: a count per report reads as total loss.
- **What moves it.** The one-way delay alone. With the delay low, the target
  rose from the floor to near the cap within about 3 s. A delay of 300 ms held
  it at the floor from the start, and after a clean start walked it from 58.4 to
  20.8 Mbit/s in steps over about 10 s, with the encoder following each step.
  Reported loss of up to 44% for 30 s, and the bandwidth estimate, moved nothing,
  though the controller showed both. A TMMBR asking for 2 Mbit/s changed nothing
  and was not answered.
- **Below the floor.** An offer capped under 20 Mbit/s pins the controller at
  its floor, and the encoder runs at the cap whatever is reported. Remotex's
  12 Mbit/s cap is such an offer, so its stream has no rate control at all.
- **Apple's viewer.** It offers up to 100 Mbit/s and four tiles, sends `RCTL`
  every 50 ms, and acknowledges each decoded tile picture with a 4-byte APP
  packet for the encoder's long-term references. On a quiet link its target sat
  at 58.4 Mbit/s with a round-trip time of about 1 ms. Its session was encrypted,
  so its reports were not read: the layout above comes from AVConference's code
  that builds and parses them, confirmed by a probe whose reports the Mac took as
  it takes the viewer's.

These are the virtual Mac's measurements, with synthetic reports. A congested
link to a physical Mac has not been observed.

### The sound

- **Codec.** AAC-ELD (MPEG-4 object type 39), whatever the offer lists: an
  offer with AAC-ELD removed was agreed and streamed AAC-ELD anyway. 48 kHz
  stereo, one 480-sample access unit per RTP packet (10 ms), payload type 101,
  about 320 kbit/s. The decoder is configured out of band with
  AudioSpecificConfig `F8 E6 50 00`: object type 39, 48 kHz, stereo, 480-sample
  frames, no SBR, no resilience tools.
- **Decoder.** Neither a browser's WebCodecs nor FFmpeg's native `aac` decodes
  AAC-ELD, so the gateway does (`src/aac_eld.rs`). It uses the pure-Rust port of
  Fraunhofer's fdk-aac decoder that AOSP ships as `platform/external/aac`,
  `rust/`, cut down to raw AAC-ELD access units
  ([fdk-aac-rust](https://github.com/andrewtheguy/fdk-aac-rust)). Against
  AudioToolbox's own AAC-ELD (`afconvert -d "aace@48000#480"`), it decoded every
  packet and matched the fixed-point C decoder to 86 dB SNR, at about 31 µs per
  10 ms unit. Fed 200 000 corrupted units with overflow checks on, it concealed
  or refused them and never panicked, which matters because the gateway aborts
  on panic. Its instance holds `Rc`s, so it runs on a thread of its own behind a
  64-unit queue.
- **Onward.** The decoder's 16-bit PCM goes to the session's audio bridge two
  units at a time, one Opus packet's worth, and from there the same way every
  target's sound goes: Opus on `/ws/audio`. The format is
  announced when the decoder opens and withdrawn when the receiver ends.
- **Authentication.** Every packet is authenticated with its leg's own
  server-to-viewer key before it is decrypted, and the reports that keep the leg
  alive go out as SRTCP, as on the picture's leg. v0.0.249, which also decoded
  this sound, stripped the tag unread and sent plain RTCP.
- **Display changes.** A change stops the sound with the picture, and the next
  offer restarts both under new SSRCs on the same ports. The receiver, and with
  it the decoder, carries on across it.
- **The virtual Mac's sound fails on its own.** On the Apple Virtualization guest,
  a looping tone at a 2x display went distorted after about a minute and then
  silent, and it did the same under Apple's own viewer. Remotex decoded it as it
  came: 100 units a second, none concealed, the RMS falling while the peak held,
  then exact digital silence. That guest is laggy whenever it plays sound, so
  judge sound quality and performance on a physical Mac. The receiver's debug log
  reports the decoded level every second, which shows whether the Mac sent a
  fault.

### RFB while the stream runs

The Mac keeps answering `FramebufferUpdateRequest`s with RFB pixels for the region
asked for, and pushes them unrequested inside the armed `AutoFrameBufferUpdate`
region.
Once a picture of the current size has arrived, remotex polls and arms one pixel.
That still brings every cursor shape and layout: 11 cursor shapes in 20 s of
moving over a TextEdit window, with 123 bytes of zlib. A login once pushed a whole
screen unasked, which is decoded to keep the deflate stream in step and not
shown.

This also ends the input freeze behind a playing video. With the gateway capped at
15% of a core, the Mac's receive queue of our input was empty in 64 of 68
one-second samples and never above 3.5 KB. Over zlib under the same cap, input
went unread for 5–19 s at a time
([Other messages](#other-messages)).

### Display changes

Every display change stops both legs, so the sound drops out with the picture
until the new stream starts. The Mac then re-sends message 1 on its own,
with no stream behind it. A new offer after the new layout starts a new stream on
the same ports, under a new SSRC, with an IDR at the new size. Remotex offers once
the resize's cover comes down, and ZRLE shows the new display until then.

### Reaching the gateway

The Mac sends from its own address to the viewer's address on the TCP connection,
so a NAT between them has to pass it. The viewer's reports go out from the same
ports every second, which opens a port-preserving NAT's mapping. Every Mac uses the
same port numbers, so remotex binds them with address and port reuse and connects
each socket to its Mac. Several gateways on one host can then share the numbers,
unless one of them bound without reuse, as v0.0.249 did. Nothing arriving within
5 s of message 1 is logged, and the session ends when the offer's first picture
is 10 s overdue.

## Still unknown

- **Apple's private framebuffer codecs**, `0x3ea` and `0x3f3`: an adaptive,
  tile-based, JPEG-like codec among them. Neither is advertised.
- **Authentication types 31–36 on the wire.**
- **Rate control's loose ends**: the second byte of `RCTL`, whether loss lowers
  the target over longer than 30 s, and whether any offer field lowers the
  20 Mbit/s floor ([Rate control](#rate-control)).
- **Four-tile frames**: how Apple's viewer places each strip.
- **Cases the test Mac could not show:**
  - a non-console user;
  - hardware mirroring;
  - a display record whose density is 0.0.

## Reproducing any of this

The measurements came from throwaway probes that speak the protocol by hand and
never call into `src/`, so a misreading on one side could not be agreed with by
the other. Three instruments did most of the work:

1. **Listing the Mac's displays while a session was live** (`CGGetActiveDisplayList`,
   run in the Mac's desktop session), to check every layout against the Mac's own
   view.
2. **Bisecting one message or encoding at a time on a fresh connection,** with
   every other session closed; a stale session invalidates display-state
   observations.
3. **A rolling log of every byte handed upward, dumped on the first parse
   failure.** Framing bugs here surface many messages after their cause.

The Mac's own view is in its unified log:

```sh
/usr/bin/log show --last 1h --style compact --predicate 'process CONTAINS[c] "screenshar"'
```

Use the full path over SSH, because zsh's `log` builtin shadows it. It records
each display selection, scaling request and connection.
