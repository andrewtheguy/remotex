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
`src/vnc_apple.rs` (Apple's messages and encodings) and the two Apple paths in
`src/vnc.rs`.

## Summary

| | |
|---|---|
| Two subtypes | `subtype = "ard"` is Standard mode: RFB 3.8, sharing the Mac's physical displays, at a fixed size. `subtype = "ard-high-performance"` is High Performance mode: RFB 003.889 with an encrypted record layer, sharing one virtual display the Mac creates at the size the client asks for. |
| Confirmed | Type-30 authentication, the record layer and its initial rekey, zlib, the cursor cache, the display layout and the metadata framing. |
| Corrected | Several published reverse-engineered descriptions are wrong on points remotex depends on: the layout's length and display count, `ViewerInfo`'s body, the virtual display's maximum size, and `AutoFrameBufferUpdate`. So are High Performance's pointer buttons and the wheel. Each is covered below. |
| Density | A virtual display is asked for at 1x or 2x only; a fractional ratio is not rounded and produces a zoomed desktop. Standard mode is scaled by the Mac to the browser's density, and a mixed-density All Displays view is composed in the browser, as Apple's viewer does. |
| Not implemented | High Performance's own system-audio stream and its HEVC video leg; Apple's controls for two virtual displays and fixed presets; authentication types other than 30. A Mac's sound reaches remotex through its AirPlay receiver instead. |

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
exercised, and only type 30 supplies the key the High Performance record layer
starts from, so remotex offers nothing else.

## Connecting

1. **Version.** `RFB 003.889` for High Performance, `RFB 003.008` for Standard.
2. **Type 30.** A Diffie-Hellman exchange. `MD5(shared secret)` is the AES-128 key
   that encrypts the 128-byte credential block (username at 0, password at 64) in
   **ECB** mode, not the CBC a published description gives. It is also the first
   key the record layer's rekey is wrapped under.
3. **ClientInit.** High Performance sends `0x81`: `0x80` asks for Apple's extended
   ServerInit, and `0x40`, never set, would ask for a session-select exchange
   remotex does not implement. Standard sends the ordinary shared flag.
4. **ServerInit** (extended on High Performance; see below), then
   `SetPixelFormat` and `SetEncodings`.
5. **High Performance only:** a cleartext prelude (`ViewerInfo`, `SetMode(control)`,
   `AutoPasteboard(start)` when clipboard is on), then `SetEncryption` commands 1
   and 2. The Mac answers with the rekey, and everything after it travels in
   records.

### ServerInit's name field is not a name

In the extended ServerInit, the "name" is 22 bytes of structure and then the
UTF-8 name: a zero `u16`, a `u32` of server flags, a 16-byte capability bitmap,
the name. Read as a name, it prints as mojibake. The flags:

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

`vnc_apple::ENCODINGS` asks for zlib from the start (a 3200×1800 frame is about
400 KB against 23 MB raw) along with the layout.

Advertising is a promise: every advertised encoding must be decodable, or at least
steppable. `CursorPos` (`0x44c`) has no payload. `DisplayInfo` is 10 bytes of
header, then `0x1c` bytes per screen. The other metadata encodings each start with
a `u16` giving how much follows.

## The High Performance record layer

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

**zlib** (`0x06`) is one deflate stream for the life of the connection. Each
rectangle is a `u32` length and a chunk of that stream, inflating to exactly
`w × h × 4`. On a static desktop it is roughly 50:1, and Standard mode compresses
on the same terms.

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
- a refresh rate (60 Hz);
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
  it is offered again. That matters to a client of the audio stream below, which
  remotex does not use.
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

### High Performance reads the pointer mask as CGMouseButton numbers

RFB's mask is bit 1 left, bit 2 middle, bit 3 right, and Standard mode honours it.
The Mac swaps bits 2 and 3 for every protocol version except 3.888 and 3.889, and
its agent reads the mask as macOS button numbers (left, right, center). So a
by-the-book right-click reaches a High Performance session as a middle-click,
which macOS does nothing visible with. `Buttons` in `src/vnc.rs` swaps the two
bits for that subtype.

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

Arming the full framebuffer at setup and after every layout is still required,
because it keeps cursor updates alive across logins and locks. Its rectangle is
not a flow-control knob: changing it mid-session corrupts later updates, except
for the one-pixel arming around a High Performance resize.

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
and `AutoPasteboard(start)`, in that order. High Performance sends them in the
cleartext prelude, and repeats `AutoPasteboard(start)` after the virtual display's
layout. The Mac then signals with `MiscStatus`:
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
| media stream | `0x3f2` | 1010 | not advertised |
| zlib | `0x06` | 6 | standard RFB |
| Raw | `0x00` | 0 | standard RFB |
| `DesktopSize` | — | −223 | pseudo-encoding |
| `LastRect` | — | −224 | pseudo-encoding |

Message types: `MiscStatus` `0x14`, `AutoFrameBufferUpdate` `0x09`, `ViewerInfo`
`0x21`, `SetDisplayConfiguration` `0x1d`, `SetDisplay` `0x0d`, `SetServerScaling`
`0x08`, and the media-stream negotiation `0x1c`.

## The media stream: High Performance system audio

> **Current remotex does not speak this stream.** A Mac's sound reaches remotex
> through the gateway's AirPlay receiver, for both Apple subtypes. The
> implementation described here — `src/vnc_apple_audio.rs`, `src/aac_eld.rs` and
> `tests/hp_audio_probe.py` — was removed after **v0.0.249**; `git checkout
> v0.0.249` recovers it. Its wire formats, captures and the decoder evaluation are
> in the archived detailed revision of this document.

High Performance carries the Mac's system audio, but not over RFB. RFB only
negotiates it: the client sends an offer in message `0x1c`, and the Mac answers
with the UDP ports and its own answer as framebuffer rectangles (encoding 1010).
The audio itself then flows as an AVConference stream — the FaceTime media stack —
over UDP with SRTP, directly to the client.

The facts a new implementation would have to meet:

- **The codec is fixed.** The Mac always sends AAC-ELD: 48 kHz stereo, 480-sample
  frames (10 ms), AudioSpecificConfig `F8 E6 50 00`. It does so whatever codec
  the negotiation agrees on; an offer of only AMR-NB and EVS was accepted and
  answered with AAC-ELD all the same. Neither browsers' WebCodecs nor FFmpeg's
  native decoder decode AAC-ELD. Apple's AudioToolbox does, on macOS only, and
  elsewhere it takes Fraunhofer's decoder, whose licence is not OSI-approved.
- **Audio needs video.** An offer without a valid screen-video offer beside it
  negotiates and is then torn down. The video leg is HEVC, and remotex never
  received or decoded it.
- **SRTP is AES-256 counter mode with an HMAC-SHA1-80 tag,** keyed from 46-byte
  masters the client sends in the offer. A receiver must verify the tag, and must
  send RTCP at least every few seconds or the Mac stops the stream. The removed
  implementation stripped the tag unverified and sent clear RTCP reports; do not
  copy either shortcut.
- **Every display change stops the stream.** The client must offer again once the
  change has settled.

The offer itself is a binary plist around AVConference's protobuf negotiation
blob, which a client without AVConference has to build byte for byte.

## Still unknown

- **Apple's private framebuffer codecs**, `0x3ea` and `0x3f3`: an adaptive,
  tile-based, JPEG-like codec among them. Neither is advertised.
- **The media stream's HEVC video leg.**
- **Authentication types 31–36 on the wire.**
- **The native viewer's protected RTCP reports.**
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
