# Apple RFB 003.889, as measured

How a Mac's Screen Sharing behaves on the wire, as far as remotex depends on it.
None of this is documented by Apple. It was measured against macOS 26.5–26.6
Apple Virtualization guests between July and September 2026, and read from the
binaries those guests ship where the wire could not show a server's rules. A
macOS update is free to invalidate any of it.

This document states behaviour and the rules remotex follows because of it. The
evidence behind it is archived outside the repository, in
`apple-screensharing-audit-2026-09-28`:
- function-level traces of Apple's viewer, `screensharingd` and
  `ScreensharingAgent`;
- captures, daemon logs and probe scripts;
- a copy of this document's earlier, fully detailed revision.

The implementation is `src/vnc_record.rs` (the 003.889 record layer),
`src/vnc_apple.rs` (Apple's messages and encodings), `src/vnc_apple_media.rs`
(High Performance's media stream), `src/aac_eld.rs` (what its sound is) and
the two Apple paths in `src/vnc.rs`.

"High Performance" below is Apple's mode: a virtual display and the media stream.
Both modes speak RFB 003.889, Apple's own revision, inside its encrypted record
layer.

| Subtype | Mode | Picture | Sound |
|---|---|---|---|
| `ard` | Standard, the physical displays | ZRLE | none; the Mac's own output is left alone |
| `ard-high-performance` | High Performance, one virtual display, or two with `virtual_displays = 2` (alpha) | HEVC over the media stream alone, a leg per display; until it is up the page says the screen is not available | AAC-ELD over the media stream |
| `ard` with `virtual_display = true` | Unofficial: Standard's picture on High Performance's one virtual display, resizes included | ZRLE | none; the Mac's own output is left alone |

`ard-high-performance` is High Performance as Apple's viewer has it. Decoding its
picture needs FFmpeg on the gateway's host; without it the gateway runs it only
with the picture passed through, for browsers that decode it. Its sound is
always passed, for the browser to decode.

**Unofficial:** `virtual_display = true` on an `ard` target keeps that row's
picture and sound — ZRLE, none — and takes the display from the other: the same
`SetDisplayConfiguration` at setup and on every resize, with no media stream
offered. Apple's viewer never offers a virtual display without the stream, so
nothing but remotex exercises the Mac's side of this combination. It was tested
against macOS 26 only, and a macOS update is free to break it while leaving both
official modes alone.

## Summary

| | |
|---|---|
| Two subtypes | Both speak RFB 003.889 with an encrypted record layer, as Apple's viewer answers every Mac. `subtype = "ard"` is Standard mode, sharing the Mac's physical displays at a fixed size. `ard-high-performance` is High Performance mode, sharing one virtual display the Mac creates at the size the client asks for, or two. |
| Confirmed | Type-30 authentication, the record layer and its initial rekey, zlib and ZRLE, the cursor cache, the display layout and the metadata framing. |
| Corrected | Several published reverse-engineered descriptions are wrong on points remotex depends on: the layout's length and display count, `ViewerInfo`'s body, the virtual display's maximum size, `AutoFrameBufferUpdate`, the type-30 credential cipher and group, and the byte order of the media stream's flags. So are the pointer buttons on this revision and the wheel. Each is covered below. |
| Density | A virtual display is asked for at 1x or 2x only; a fractional ratio is not rounded and produces a zoomed desktop. Standard mode is scaled by the Mac to the browser's density, and a mixed-density Combined Display view is composed in the browser, as Apple's viewer does. |
| Picture and sound | `ard` is ZRLE throughout, and carries no sound: Standard mode never touches the Mac's sound output. `ard-high-performance` takes both from the media stream, as Apple's viewer does — HEVC and AAC-ELD over SRTP. Its ZRLE rectangles are stepped over unread and never shown, so a session that passes the stream builds no video encoder: until the stream is up and across display changes the page says the screen is not available and sends the Mac no input. |
| Not implemented | Apple's fixed resolution presets; its viewer's rate feedback on the media stream; authentication types other than 30. |

## Remote Management access

Rule out the Mac's Remote Management permissions before treating an
authentication failure as a protocol fault. When the account lacks permission,
the type-30 exchange completes before the Mac refuses, exactly as it does for a
wrong password. Both Apple subtypes then report
`VNC authentication failed: the Mac refused the login (result 1)`, which does not
tell the two causes apart. The Mac sends no reason with the refusal (see
[Other login types](#other-login-types)).

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

A Mac also offers other login types, and Apple's viewer tries some of them before
type 30 (see [Other login types](#other-login-types)). Only type 30 has been
exercised, so remotex offers nothing else.

## The two modes in Apple's viewer

Apple's viewer's connection sheet offers **Standard**, with a choice of Adaptive
or Full quality, or **High Performance**, with one or two virtual displays. The
choice is made after ServerInit; the handshake before it is the same in both
modes (see [Connecting](#connecting)).

| | Standard | High Performance |
|---|---|---|
| Encodings | Adaptive: Apple's private `0x3f3` and `0x3ea`, then zlib and ZRLE. Full: zlib, then ZRLE. | The media stream (1010), then as Adaptive. |
| Displays | The physical ones, selected with `SetDisplay`, scaled with `SetServerScaling`. | One or two virtual displays from `SetDisplayConfiguration`, 60 Hz unless a preference says otherwise. |
| Quality menu | Adaptive or Full, selecting RFB framebuffer encodings. | No choice. The control is disabled while the media stream runs; its separate rate controller is automatic. |
| Refused beside it | A virtual display, dynamic resolution, HDR. | No virtual display, or a screen other than the virtual ones. |

Standard's **Adaptive** label does not name a High Performance setting. The
High Performance video profile always enables AVConference's rate adaptation,
with fixed 20 and 60 Mbit/s bounds, whether or not its reports can make the
encoder move between them. The viewer exposes no switch for it. See
[Rate control](#rate-control).

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
displays, offers no resize and never creates a virtual display, and
`ard-high-performance` creates one, the "1 Virtual Display" choice, or with
`virtual_displays = 2` the "2 Virtual Displays" one
([Two virtual displays](#two-virtual-displays)), and
never selects a physical screen or sends `SetServerScaling`. It departs in two
places, and a third unofficially — `ard` with `virtual_display = true` creates
the one virtual display the way High Performance does and then runs Standard's
ZRLE session on it, a combination the viewer never offers (above):
- **Encryption.** Apple's viewer asks for the record layer only when its
  `encryptionLevel` preference is 2. The default is 0, which leaves the whole
  session in cleartext after authentication, keystrokes and the media stream's
  keys included. Remotex always asks, in both modes.
- **Standard's picture.** `ard` asks for ZRLE alone, where Full quality asks for
  zlib first and Adaptive first for
  [private codecs](#apples-own-framebuffer-encodings) remotex does not decode.

## Connecting

Both modes connect the same way until the record layer is up.

1. **Version.** `RFB 003.889`, as Apple's viewer answers every Mac. It sends
   `003.003` to a server that is not a Mac.
2. **Type 30.** A Diffie-Hellman exchange. The Mac sends a `u16` generator, a
   `u16` key length, the prime and its public key; macOS 26 sends RFC 5054's
   4096-bit prime with generator 5, so both keys are 512 bytes, not the 1024-bit
   group with generator 2 a published description gives. Apple's viewer takes key
   lengths from 64 to 1024 bytes and refuses any other; remotex takes the same.
   The viewer answers with
   the 128-byte credential block (username at 0, password at 64), then its public
   key. `MD5(shared secret)` is the AES-128 key that encrypts the block in **ECB**
   mode, again not the published CBC. It is also the first key the record layer's
   rekey is wrapped under. A refusal is the result word alone (see
   [Other login types](#other-login-types)).
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
   screen being sent. Both arm it for one push a video frame at most (see
   [Other messages](#other-messages)).

### Other login types

Remotex speaks only type 30. This is what the Mac's code does with the others;
only the list and type 30 were measured.

**The list.** To an `RFB 003.889` viewer the Mac lists, in this order:
- 30, always;
- 33 (RSA), always;
- 36 (SRP), unless the Mac has network directory nodes and a preference does not
  allow SRP for them;
- 31 and 32 (asking the Mac's user), when **Anyone may request permission to
  control screen** is on;
- 2 (the VNC password), when **VNC viewers may control screen with password** is
  on, or before the Mac's first setup has finished;
- 35 (Kerberos), when the Mac can do Kerberos and a preference does not disable it.

The test Mac sent `04 1e 21 24 23`: 30, 33, 36 and 35. A Remote Management
preference can replace the list with one type, and a connection the Mac was told
to expect gets 34 alone. The Mac refuses a type it did not list.

**The result.** On success the Mac sends SecurityResult `u32` 0. A refusal is the
`u32` alone: the Mac sends its reason string only to an RFB 3.8 viewer. The Mac
then left the connection open for the 90 s a test waited; its code closes it
when the viewer next writes.

**Each type**:
- **31 and 32** run type 30's exchange, but the Mac ignores the name and password
  and asks its user to let the viewer in, to observe (31) or to control (32). The
  Mac refuses keyboard and mouse input from a viewer let in by 31.
- **33** starts with a `u32` length, then an envelope: a non-zero `u16` version,
  `RSA1`, a `u16` kind, and the kind's body.
  - **Kind 0** asks for the Mac's RSA public key. The Mac answers with a `u32`
    length, `00 01 00 00`, a `u16` n, n bytes of DER, and a zero byte. When it
    cannot decrypt a later envelope, it answers with the key in the same form,
    without the zero byte.
  - **Kind 1** is a plain login. Its body is type 30's 128-byte credential block,
    under AES-128-ECB with a key the viewer chose, then a little-endian `u16` 256
    and that key encrypted to the Mac's public key. The Mac answers `u32` 0, then
    the result. The viewer's key is the one the record layer's rekey is wrapped
    under.
  - **Kind 2** carries SRP, and the Mac takes it only when it also listed 36.
    The body is a `u16` length and SRP's first message, encrypted to the Mac's
    key; the second message goes in clear in the same envelope. The Mac answers
    each with a `u32` length, `00 00 00 02`, a `u16` n and n bytes of SRP, and
    then sends the result.
- **36** is the same SRP without RSA: after the selector, each message is a `u32`
  length and SRP bytes in clear, both ways.
- **34** is for a connection the Mac was told to expect, with a 16-byte key
  both sides hold beforehand. The Mac sends a 16-byte challenge under AES-ECB
  with that key, checks the viewer's 16-byte answer, sends 16 bytes back, and
  then sends the result.
- **35** starts with the viewer's `u32` 0 and the Mac's `u32` answer. Kerberos
  tokens follow (not traced), for the service `vnc`. On success the Mac makes a
  random 16-byte key and sends it as a `u32` length and the key, wrapped by the
  Kerberos context. That key is the one the rekey is wrapped under.

**SRP.** Each SRP message is a `u32` length, then fields:
- a `u8`;
- big numbers with a `u16` length;
- opaque values with a `u8` length;
- strings with a `u16` length;
- a `u64`.

The messages:
1. The viewer sends an empty string, the user name, an empty string and an empty
   opaque value.
2. The Mac answers a zero byte, N, g, the salt, B, the PBKDF2 iteration count
   and an options string (`mda=SHA-512,replay_detection,conf+int=ChaCha20-Poly1305,kdf=SALTED-SHA512-PBKDF2`).
   N and g are RFC 5054's 4096-bit group, and the hash is SHA-512. An account
   that does not exist still gets an answer, with a random salt.
3. The viewer sends A, its proof M1, the options string again, and an opaque
   value.
4. The Mac answers its proof M2, an opaque value, an empty string and a `u32` 0.

The rekey is then wrapped under the first 16 bytes of the SHA-256 of the SRP
session key. The ChaCha20-Poly1305 in the options string plays no part in the
record layer.

**Apple's viewer**, logging in with a name and password, tries 33, then 36, then
30, with Kerberos first or after them depending on its own preference. Asking
for permission, it tries 32, then 31. Of 33 it sends the SRP form to a Mac that
lists 36, and the plain login to one that does not.

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

A message can span records: a full-screen zlib rectangle is about 400 KB
against a record ceiling of 65,520 bytes, so records are reassembled by
concatenation, never read one message per record. Remotex frames its own messages
the same way: a pasteboard archive past one record's 65,498-byte body goes out as
full records and then the rest.

**The rekey** arrives as a one-rectangle framebuffer update with encoding `0x44f`
and zero geometry. Its body is a `u32` generation, then a wrapped key and a
wrapped IV, each one AES-128 block decrypted under the wrap key. The Mac rotates
keys only when the viewer asks with `SetEncryption` command 1, and it switches
both of its ciphers the moment it sends a rekey. Remotex asks once, during setup.
It closes the session on any later rekey rather than follow it, because records
it had already framed under the old key would fail the Mac's check.

**`SetEncryption` (`0x12`)** is a type, a pad byte, a `u16` command and command
words:
- **Command 1** is followed by a `u16`, a `u16` count of at most 100, and that
  many `u32` methods. One of them must be 1, and the Mac then draws a fresh random
  key and IV and sends the rekey. Remotex sends `12 00 0001 0001 0001 00000001`.
- **Command 2** is followed by a `u16` and a pad. A 1 makes the Mac decrypt what
  it receives from then on; any other value turns that off. It does not stop the
  Mac encrypting what it sends. Remotex sends `12 00 0002 0001 0000`.

A published description reads the two commands as start and stop.

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

**The cursor can stay the arrow while a selection tool is up on the Mac.** A
tool that takes over the screen to let the user pick something gives the Mac's
own cursor a new shape, and a remote session keeps showing the shape from before
it. The tool itself still works: clicks and moves go in as usual. Apple's own
viewer does the same, so this is not a remotex fault and there is nothing to fix.

The cause is on the Mac. Its cursor changes as soon as the tool starts, but it
sends no cursor shape while the tool is up. The new shape goes out only when the
tool ends, after `MiscStatus` 12, with the ordinary shape right behind it. The
Mac re-reads its cursor when it sees the mouse move, and by all appearances such
a tool keeps those moves to itself. Nothing a viewer sends makes the Mac send the
shape sooner. Cursors that change on hover, such as the hand over a link, are
unaffected.

Seen so far with the camera of Shift-Command-4's window selection and of
QuickTime's screen recording. For another tool, check Apple's viewer first: if
the cursor stays the same there too, it is this.

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

Standard shares physical displays at a fixed size: it offers no resize and
never sends a viewport size. It does honour a scale.

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

### Combined Display over mixed densities

No single factor renders a 1x screen beside a 2x one. Apple's viewer does not try.
In Combined Display over mixed densities it never sends `SetServerScaling`; it takes
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

This is the one place the browser rescales remote pixels (see
[Input and display](architecture.md#input-and-display)).

At factor 1.0 the combined framebuffer is the screens' native pixels side by side,
and it is often past what a video stream encodes: a 2x screen beside a 1x one
measured 5376×2287. Standard never resizes, so the gateway
cannot ask for less. Such a view has no picture: the session stays up, and the
page offers the Mac's screens instead, since one screen is a smaller desktop.
Choosing one within the ceiling returns the session to video at the next layout.
Combined Display over more than two screens is held the same way whatever its size or
densities, since composing them is too much for a browser to draw. See
[past the ceiling](architecture.md#past-the-ceiling).

### The High Performance virtual display

High Performance hides the Mac's physical screens and moves every window to a
virtual display. Remotex creates one at the size the session keeps, the target's
`size` or the default, or in a session started with resize at the full size of
the browser's screen, and at that screen's density either way, as Apple's client
does. The window layout macOS produces depends on that opening size:
windows squeezed onto a small opening display do not spread out again when it
grows. In a session started with resize, each viewport report asks for a new
size, and the next layout confirms it. Everything in this section and the next holds as well
for the unofficial `virtual_display = true` under `ard`, which sends the same
messages; on macOS 26 the Mac answered them the same way with no media stream
offered, and that is the only macOS it was tried on.

`SetDisplayConfiguration` (`0x1d`) carries a display count and, for each display,
one descriptor with one mode:

| Descriptor field | Value remotex sends |
|---|---|
| name | 120 bytes, UTF-8 and zero-filled: `Screen Sharing Virtual Display`, and `Screen Sharing Virtual Display #2` for the second, which are the names Apple's viewer's displays have. It is what the Mac calls the display, in its Displays settings and to its applications; left empty, the displays have no name and the Mac lists them as ` (1)` and ` (2)`. |
| display flags | 1: dynamic resolution. Bit 1, never sent, tells the Mac to leave the refresh rate alone and ignore the mode's. |
| display type | 4, virtual |
| physical size | millimetres, as big-endian `f32` |
| maximum backing size | 3840×2160, a fixed ceiling |
| rotations | 7, Apple's captured value. The agent hands it unchanged to macOS as the virtual display's rotations setting. |
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
- **The first descriptor is always dynamic,** even in a session started without
  resize, so a reconnect re-enables the Mac's Dynamic resolution setting. Resize
  controls only whether remotex acts on later viewport reports.
- **The display outlives its session briefly.** A reconnect within a few seconds
  finds it still there with the same id; after about 45 seconds the Mac is back on
  its physical display. The new session's own layout arrives either way.

### Two virtual displays

Apple's viewer's High Performance sheet offers "2 Virtual Displays", and a
target's `virtual_displays = 2` (alpha) asks the Mac for the same. What that
changes, as read from Apple's viewer and daemon and as macOS 26 answered:

- **The Mac says how many it creates.** Its ServerInit flags carry the count
  above bit 4: 2, unless a managed preference holds the Mac to 1. It holds a
  configuration that names more to that many without saying so. Remotex refuses
  a session that asks for more than the Mac states, before it sends one.
- **One configuration names both.** The display count is 2 and the descriptors
  follow back to back, each led by its own length, which is how the Mac steps
  from one to the next. Remotex sends the one-mode descriptor above for each
  display, in the opening configuration and in every resize, the second's at
  the density of the screen its tab's window is on once that tab has reported
  one, and at the first's until then. The Mac creates the
  second display to the right of the first, top-aligned: 1440×900 beside
  1440×900 put it at 1440 points across, and 1366×768 beside 1024×700 at 1366.
  Nothing known in the descriptor says where; the arrangement is changed on the
  Mac, in System Settings → Displays → Arrange, or by any program in its desktop
  session through `CGConfigureDisplayOrigin`. A later configuration keeps an
  arrangement made that way, and a new session sometimes opened with it and
  sometimes with the second display on the right again.
- **The layout lists both under the combined sentinel,** the first display
  first, and its framebuffer is the span of the two: 2880×900 over two 1440×900
  displays. The records stay in that order however the displays are arranged,
  and their corners are the framebuffer's, never negative: with the second
  display dragged above a 1440×900 first, the first's record was at 0,900 and
  the second's at 0,0. Each record's backing rectangle is where that display sits in the
  span, and pointer positions are addressed in the span too. A position of
  100,200 on the second of those displays, sent as 1540,200, put the Mac's
  pointer at 1540,200; over two 1280×800 displays at 2x, 2660,200 put it at
  1330,100 in points. A position sent in the first second after the layout
  moved nothing, on one display as on two.
- **The media stream has a video leg per display.** Message 1 enables video 2,
  on the port after video 1's; the offer carries the second display's keys and
  offer behind the first's; the answer has a blob for each
  ([Negotiation](#negotiation)). The legs follow the Mac's arrangement, not the
  order the displays were asked for in: video 1 is the display that starts the
  spanned framebuffer. With the second display where the Mac creates it, to the
  right, that is the first: asked for 1366×768 beside 1024×700, each leg's
  pictures were its own display's size. With the second dragged to the left of
  the first or above it, in the Mac's Displays settings, video 1 carried the
  second display's pictures, and remotex reads each leg as the display the
  layout places there (`MediaStream::arrange`). A diagonal arrangement has not
  been tried. Each leg has its own keys, SSRC, reports, rate feedback and
  keyframe requests, and the offers carry the session's one call id, as Apple's
  viewer's do. The legs are offered, answered, stopped by a display change and
  offered again together, in one message each time, so the rule of one offer at
  a time is unchanged.
- **Remotex shows one display on a page.** The picker lists `Display 1`,
  `Display 2` and *All Displays*, which two displays start on and which keeps the first on the session's page
  and shows the second in a browser tab of its own, where Apple's viewer opens a
  window for each. The choice is answered in the gateway: the Mac sends both legs
  whatever is chosen, and the leg of a display nobody is shown is authenticated,
  counted for liveness and dropped. A display coming into view starts at a
  keyframe the Mac is asked for with a PLI on its leg. In a session started with
  resize the tab's window sizes the second display, and otherwise both are the
  size the session keeps. See
  [Display geometry](architecture.md#display-geometry).
- **Each display owes its first picture.** An offer for two displays ends the
  session when either leg brings none within the 10 s, or goes silent
  ([Liveness](#the-stream)). The first packet of a picture stands for the first
  picture of a display nobody is shown, since none of its pictures is put
  together.

Checked on macvm only, decoded and passed, at 1x and 2x. HDR on either display,
a physical Mac, and what Apple's viewer does beside it have not been.

### Resizing a High Performance display, as measured

Three behaviours of the Mac shape how remotex resizes:

- **A display change stops the Mac's media stream.** The Mac restarts nothing until
  it is offered again, so there is no picture until the new display's stream
  delivers: remotex sends `screenUnavailable`, and the page says the screen is
  not available — see [Display changes](#display-changes).
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
  same lock (see [Other messages](#other-messages)).

## Input

### Apple's revision reads the pointer mask as CGMouseButton numbers

RFB's mask is bit 1 left, bit 2 middle, bit 3 right. The Mac swaps bits 2 and 3
for every protocol version except 3.888 and 3.889, and its agent reads the mask as
macOS button numbers (left, right, center). So on 003.889 a by-the-book
right-click arrives as a middle-click, which macOS does nothing visible with, in
either mode. `Buttons` in `src/vnc.rs` swaps the two bits for both Apple subtypes.

### A Mac scrolls by a distance

The wheel bits of the pointer mask are a poor scroll on a Mac. It scrolls only for
a mask of exactly `0x08` (up) or `0x10` (down), about two pixels a pulse. Any other
combination is posted as buttons: a wheel bit with a button held becomes Back or
Forward, and the horizontal bits `0x20`/`0x40` become clicks on buttons 5 and 6.

Apple's viewer sends a scroll in a message of its own instead, the scroll-wheel
event of the second event message (`0x17`), and so does remotex, in both modes
(`vnc_apple::scroll_wheel`). It is 58 bytes, numbers big-endian:

| Bytes | |
|---|---|
| 0 | type, `0x17` |
| 1 | flags, 0 |
| 2–3 | `u16` size of what follows, 54 |
| 4–5 | `u16` version, 1 |
| 6–7 | `u16` kind, 11: a scroll-wheel event |
| 8–13 | three `i16` line deltas |
| 14–25 | three `i32` line deltas in 16.16 fixed point |
| 26–37 | three `i32` point deltas |
| 38–41 | `u32` scroll phase |
| 42–45 | `u32` momentum phase |
| 46–49 | `u32` scroll count |
| 50–53 | `u32` flags: 1 instant mouser, 2 continuous, 4 inverted from the device |
| 54–57 | `u16` x and `u16` y, the pointer's position |

Each group of three is horizontal, vertical, then a third axis, and a scroll up or
to the left is positive, the opposite of the DOM's. The agent copies every field
into the `CGEvent` it posts, so an application receives what was sent: remotex
sends the distance as the point delta, a tenth of it as the line delta, the
continuous flag, and no phase, which an application reads as a precise scroll
with no gesture around it. Checked on macOS 26.6 in both modes with a window that
logs its scroll events: 40 points right and 25 up arrive as exactly that, at the
pointer.

Apple's viewer sends this message only to a Mac whose ServerInit lists `0x17`
([the command bitmap](#serverinits-name-field-is-not-a-name)), and falls back to
wheel bits otherwise. Remotex has no such fallback: an Apple subtype refuses the
session on a Mac that does not list it.

### Keys

- **Modifiers follow the keysym.** An uppercase letter brings Shift with it, and a
  held Shift is stripped from a lowercase one. A shortcut under Caps Lock must
  therefore be sent as the lowercase keysym, or Command-Z arrives as
  Command-Shift-Z.
- **Keys with no mapping.** Insert, Pause, Scroll Lock, Print and Menu have none on
  the Mac, and Num Lock arrives as Keypad Clear.
- **Option.** Option is stripped from ordinary keys unless Command is also held.
- **Modifier keysyms.** The agent maps modifiers by its own table, in both modes.
  `Meta_L`/`Meta_R` land on Option. `Alt_L`/`Alt_R`, `Super_L`/`Super_R` and
  `Hyper_L`/`Hyper_R` all land on Command. Each keeps its side. A by-the-book Alt
  therefore arrives as Command, so remotex sends a keyboard's Alt keys as Meta
  (`keymap::apple_keysym`, and [VNC](architecture.md#vnc)).

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
  a second. The daemon pushes once the interval has passed since its last update
  finished, so any interval above 0 leaves its sender idle that long after each
  one.
- **Apple's viewer arms a running session with 0,** in both of its modes. It
  sends the all-ones value below only while the session is paused, and no
  interval in between.
- **The Mac throttles itself, by what the connection drains.** Each time it
  sends, the daemon estimates the link's bytes a second from how fast its socket
  empties, and holds the next update, pushed or asked for, while more than a
  tenth of that is still queued. The viewer does no pacing of its own.
- **A physical display needs the pushes.** In Standard mode on a physical display
  an incremental request alone is answered late: armed at 1,000,000, a scrolling
  window drew 2–9 updates a second with the Mac silent for a second at a time,
  where the same scroll on a virtual display drew 20–30. With the interval at 0
  the physical display is smooth.
- **Remotex arms with 33,333 or more.** 33,333 is one frame of its video stream:
  it shows no more than a frame in that time however often the Mac pushes, and
  the gap after each update is when the Mac reads its input (below). A Standard
  session widens the gap as it falls behind. It arms again with what an update
  costs it to take — the time to read it, decode it and hand it to the video
  stream, the browser's link included where that holds the stream — smoothed over
  a few updates, up to 1,000,000, and comes back to 33,333 as the cost falls. It
  arms again only when the interval moves by half, and at most twice a second.
  High Performance arms 1,000,000 and keeps it: its picture is the media stream,
  and the Mac's pixel updates are stepped over unread, so one a second is the
  least it can be made to push while still arming the cursor shapes.
- **`0xffffffff` turns the pushes off.** The daemon records whether the word is
  the all-ones value and pushes nothing while it is. A published description reads
  the word as a screen id, with all-ones meaning all displays. It is not one:
  `SetDisplay` selects the screen.

Unpaced pushes cost a client its input. The Mac
[reads nothing while it writes an update](#resizing-a-high-performance-display-as-measured),
and at interval 0 pushed updates follow one another for as long as the screen
changes. A client that drains the connection more slowly than the Mac captures — a
busy gateway, a slow link — has its clicks, keys and display changes left unread
for the length of the animation. On a Mac playing a video, input went unread for
35–104 s at a time and then arrived as hundreds of queued events in one second.
Apple's viewer never meets this in High Performance mode: it takes the picture
from the media stream (encoding `0x3f2`), and the daemon's framebuffer sender
skips such a viewer. Remotex does too once the stream is up, and then arms and
polls one pixel — see [RFB while the stream runs](#rfb-while-the-stream-runs).

The interval is what keeps the input moving everywhere else. With the gateway
held to 15% of a core, Standard mode on a display of the Mac's own, an animation and a
scrolling window on the Mac and the pointer sweeping, sampled once a second:

| Interval | Seconds with input unread, of about 53 | Most unread | Picture |
|---|---|---|---|
| 0 | 47 | 41,616 bytes | 4.8 Mpx/s |
| 33,333 | 4 | 1,190 bytes | 4.7 Mpx/s |
| 1,000,000 | 2 | 2,856 bytes | 1.4 Mpx/s |
| following the cost | 3 | 3,230 bytes | 4.1 Mpx/s |

Following the cost, that gateway armed between 50,000 and 240,000. Unheld, it
never left 33,333.

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

A mis-sized body makes the Mac swallow the next message and hang silently. A
version other than 1 is only logged.

The daemon reads two bits of the capability bitmap, and reads both as clear until
a `ViewerInfo` arrives:
- **Bit 20:** it checks this bit before sending every `MiscStatus` (`0x14`)
  except command 17.
- **Bit 21:** it checks this bit before forwarding an accessibility message from
  the agent.

A published description says it reads bit 20 alone.

**`MiscStatus` (`0x14`)** is `14 00 00 04 00 01` and a `u16` command:

| Command | Sent when |
|---|---|
| 1 | the Mac's user ends the session, just before the Mac closes it |
| 2 | the Mac's pasteboard changed |
| 3 | the Mac needs data for a flavor the viewer promised |
| 4 | a 2.1 s timer finds nothing sent to the connection for 2 s |
| 5, 6 | the Mac's displays go to sleep, and wake |
| 9 | control is allowed, on a `FramebufferUpdateRequest` |
| 10 | only observing is allowed, on `ViewerInfo` |
| 11, 12 | the pointer is hidden, and shown again |
| 13, 14 | the Mac's two busy-cursor notifications |
| 17 | the Mac's user session changed |

A published description has 12 as the heartbeat and 11 as the user session
changing. The heartbeat is 4, 11 is the pointer hiding, and the session change is
17 (`0x11`). Remotex acts on 2 and 3 and steps over the rest.

**`SetMode` (`0x0a`)** is a type, a pad byte and a `u16` mode:
- **0:** observe;
- **1:** control;
- **2:** control with the Mac's own keyboard and mouse inhibited, where the
  connection may do that.

The Mac refuses a mode above 2, and ignores 1 and 2 on a connection limited to
observing. The mode also sets how the Mac's Screen Sharing menu shows the session:
observed, assisted or controlled. Remotex sends 1.

**The pasteboard.** `AutoPasteboard` (`0x15`) is eight bytes with a `u16` at
byte 2: 1 starts the agent watching the Mac's pasteboard, 2 stops it, and any
other value is ignored. Change notifications need `ViewerInfo`, `SetMode(control)`
and `AutoPasteboard(start)`, in that order. Both modes send them in the cleartext
prelude, and High Performance repeats `AutoPasteboard(start)` after the virtual
display's layout. The Mac then signals with `MiscStatus`:
- command 2: its pasteboard changed;
- command 3: it needs data for a promised flavor.

Contents travel as a zlib archive (level 9, one sync flush, capped at 100 MB) of
every flavor of every item. A short text selection can therefore arrive inside
megabytes of other flavors. Remotex streams the archive, keeps only the text, and
sends empty text as an item with no flavors, which clears the Mac's pasteboard.

- **The fetch** (`0x0b`) is eight bytes. Bit 0 of byte 1 asks for promises only,
  and the Mac honours it only while `AutoPasteboard` is started. Bytes 4–7 are
  the viewer's to choose; the Mac echoes them.
- **The Mac's reply** (`0x1f`) has a 16-byte header:
  - `1f 00`, then the fetch's promises bit in byte 2 and a pad byte;
  - the echoed four bytes;
  - big-endian `u32` uncompressed and compressed sizes;
  - then the compressed archive.

  A published description calls bytes 4–7 reserved.
- **The viewer's `0x1f`** has the same header. Bit 0 of byte 2 marks the contents
  as promises, again only while `AutoPasteboard` is started, and the Mac ignores
  bytes 4–7. A size over 100 MiB closes the connection.
- **The archive** is a run of items. Each item is a `u32` flavor count, then
  that many flavors. A flavor is a counted name, a reserved `u32`, a `u32` count
  of counted key and value tags, and counted data, every count a big-endian
  `u32`. A flavor with no data is a promise, and an empty archive clears the
  pasteboard. A published description reads the first count as the number of
  items, each holding one flavor.

**Polling pauses behind a fetch.** Framebuffer and pasteboard replies share one
ordered stream. While a pasteboard fetch is pending, remotex pauses incremental
polling, so the fetch is not stuck behind a stream of updates.

**Metadata arrives only as rectangles.** The layout, keyboard source, vendor
keysyms and device information are each a one-rectangle framebuffer update.
Apple's viewer closes the connection on a server message type it does not know,
so a reader that falls out of step sees "messages" that are really fragments of
these. Remotex reads two of them only to step over them:
- **Vendor keysyms** (`0x453`) are a fixed table: `u16` 20, then a `u16` version
  (1), a `u16` count (4), and the keysyms `0x1008FD00` to `0x1008FD03`.
- **Keyboard source** (`0x455`) is a `u16` giving the name's length plus 8, then
  a `u16` version (1) and a `u32` flag. After those come a `u16` length and the
  Mac's current input source as UTF-8, such as `com.apple.keylayout.ABC`. The
  flag is 1 while the Mac's keyboard focus is in a secure text field, such as a
  password prompt.

### Messages remotex does not use

Read from the daemon, for a reader of Apple's viewer's captures:
- **`DeviceInfo` (`0x456`)** is a metadata rectangle of zero geometry. It holds,
  in order:
  - a `u16` size of what follows, then `u16` 2, `u32` 1 and a `u32` 0;
  - three `u16` string lengths, each counting its NUL;
  - the Mac's model identifier (`hw.model`, or `unknown`) and two colour strings;
  - a big-endian `u32` housing colour, when the Mac reports one.

  A published description has the housing colour always present.
- **`EncryptedInputEvent` (`0x10`)**, from the viewer, is 18 bytes: a type, a
  flag byte, and one AES block the Mac decrypts in ECB under the key
  authentication produced. Two markers in the block say what it carries, and a
  marker other than 0 or `0xff` is a decryption error:
  - **a key,** when byte 0 is `0xff`: byte 1 is the down flag and bytes 2–5 the
    keysym;
  - **a pointer event,** when byte 10 is `0xff`: byte 11 is the button mask and
    bytes 12–15 are x and y as `u16`s.

  Numbers are big-endian.
- **`SetKeyboardInputSource` (`0x1a`)**, from the viewer, holds:
  - a type and a pad byte;
  - a `u16` size of what follows and a `u16` version;
  - a `u16` length and an input source ID, which the Mac hands to its agent.

  A published description leaves out the pad byte.

### Apple's own framebuffer encodings

Remotex advertises none of these, but Adaptive quality lists `0x3f3` and `0x3ea`
first, so a capture of Apple's viewer is full of them. This is read from the
Mac's encoders and the viewer's decoders.

**`0x3e8`, `0x3e9` and `0x3ea` are zlib with fewer bits per pixel.** A rectangle
is a `u32` length and that many bytes of zlib. Each encoding keeps its own deflate
stream from one rectangle to the next, as RFB's zlib (`0x06`, the fourth row)
keeps one. It inflates to packed rows, each starting on a byte:

| Encoding | Pixel | Row | Deflate level |
|---|---|---|---|
| `0x3e8` | 1 bit, most significant first: 1 is white, 0 black | (w + 7) / 8 bytes | 9 |
| `0x3e9` | 4-bit grey, high nibble first: 15 is white | (w + 1) / 2 bytes | 6 |
| `0x3ea` | big-endian `u16`, RGB 5-5-5 below an unused top bit | 2w bytes | 1 |
| `0x06` | the negotiated pixel format | w × bytes per pixel | 1 |

The Mac's grey is (5R + 9G + 2B) / 16. For `0x3e8` it thresholds that grey along
each row and carries half the error to the next pixel, so the picture is dithered.

**`0x3f3` codes the picture in 8×8 tiles.** A rectangle is a `u32` length, at
most 100,000,000, then a body whose first byte says what it is:
- **0, a full update.**
  - Bytes 1 and 2 are parameters for its DCT tiles.
  - Bytes 3–5 are a big-endian `u24` offset from the body's start to a data stream.
  - A command stream starts at byte 6.
- **1, a partial update,** which refines the tiles of the last full update.
  - Bytes 1 and 2 are DCT parameters; the Mac sends 14 and 19.
  - One bit stream starts at byte 3 and gives each tile a 2-bit code: 0 leaves the
    tile alone, 1 refines its DCT coefficients, 2 repeats the copy the full update
    made for it, and 3 takes a cached tile.
  - The stream ends with `mvs` (`0x6d 0x76 0x73`).
- **2, the quantization tables:** exactly 129 bytes. The 2 is followed by 64
  luminance entries and 64 chrominance entries, one byte each, in a 0×0 rectangle
  at 0,0.

The viewer decodes `0x3f3` only into 32-bit pixels, and reads each stream most
significant bit first.

A full update's command stream starts with one bit the viewer skips. Then it runs
through the rectangle's tiles in rows, left to right and top to bottom, and edge
tiles are clipped. Each step is a 3-bit command followed by a repeat count, and
the command covers that many tiles:
- **a `0` bit:** one tile;
- **a `1` bit and a 4-bit n below 15:** n + 2 tiles;
- **`1`, `1111` and a base-128 number v:** v + 17 tiles. The number is least
  significant group first, with bit 7 continuing, in at most three bytes.

A command's operands come from the data stream:

| Command | Tile |
|---|---|
| 0 | white |
| 1 | a copy of the previous tile |
| 2 | a copy of the tile above |
| 3 | black and white: an 8-bit row mask, then an 8-bit pixel mask for each row whose bit is clear. A set bit is white. |
| 4 | one or two colours. The first bit says two; the second says to reuse the colours last read in this update instead of reading new ones. A colour is 8-bit Y, then the top six bits of Cb and of Cr. Two colours are followed by command 3's masks, a set bit taking the first colour. |
| 5 | DCT-coded |
| 6 | a cached tile, by a 16-bit index |
| 7 | the cached tile after the last one used |

Both streams end with `0x6d`.

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
| 1-bit zlib | `0x3e8` | 1000 | not advertised |
| 4-bit grey zlib | `0x3e9` | 1001 | not advertised |
| RGB 5-5-5 zlib | `0x3ea` | 1002 | not advertised |
| 8×8 tiles | `0x3f3` | 1011 | not advertised |
| ZRLE | `0x10` | 16 | standard RFB |
| zlib | `0x06` | 6 | standard RFB, not advertised |
| Raw | `0x00` | 0 | standard RFB |
| `DesktopSize` | — | −223 | pseudo-encoding |
| `LastRect` | — | −224 | pseudo-encoding |

Message types: `MiscStatus` `0x14`, `AutoFrameBufferUpdate` `0x09`, `ViewerInfo`
`0x21`, `SetDisplayConfiguration` `0x1d`, `SetDisplay` `0x0d`, `SetServerScaling`
`0x08`, the scroll-wheel event `0x17`, and the media-stream negotiation `0x1c`.

## The media stream: High Performance's picture and sound

In High Performance mode Apple's viewer takes neither its picture nor its sound
from RFB. RFB only negotiates a media stream: the viewer sends an offer, and
`ScreensharingAgent` then sends the screen and the system audio through
AVConference — the FaceTime media stack — as HEVC and AAC-ELD over UDP with SRTP,
straight to the viewer. Remotex does the same on an `ard-high-performance` target
(`src/vnc_apple_media.rs`). The stream alone is the picture. The Mac's ZRLE
rectangles are stepped over by their length, never inflated and never encoded,
so a session that passes the stream builds no VP9 encoder, with the worker
threads and frame buffers one holds for a session's life. Until the stream
delivers, at connect, across display changes and across a stream the Mac
restarts, the gateway sends `screenUnavailable` and the page says "Screen not
available" over the canvas and sends the Mac no input, since nobody can see
what it would do. The notice comes down behind the stream's first
picture of the display. A stream that fails ends the session, as it
ends Apple's viewer's: one the Mac refuses, one that brings no picture or no
sound, and one that stops (see [Liveness](#the-stream)).

Remotex decodes the picture and encodes it as VP9, unless the session was
started with the picture passed through, which the picker offers a browser that
decodes the Mac's HEVC: then each access unit goes to the browser as it came,
described by the stream's own sequence parameter set, and a PLI is its repaint.
The sound is never decoded here: in every session each sound unit goes on
`/ws/audio` as it came, described by the AudioSpecificConfig below. Either
way the stream's gaps show nothing: ZRLE's rectangles are stepped over, the
page says the screen is not available, and the stream comes back at an IDR. See
[Apple's media stream, passed through](architecture.md#apples-media-stream-passed-through).

The one decoder is FFmpeg's HEVC decoder for the picture (libavcodec,
LGPL-2.1-or-later), loaded from the system's shared libraries when a session
needs it, so published release artifacts do not link it; the
`apple-hp-media-static` Cargo feature links it statically instead. A gateway
that finds it missing ends a session started without the passthrough before it
dials the Mac.

### Negotiation

After the first layout the viewer sends a second `SetEncodings`, the opening list
with encoding 1010 (`0x3f2`) appended. The Mac answers it with message 1, below,
naming its ports, and the viewer makes its offer only then: message `0x1c`
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
+0x80      audio offer, then the video1 keys (46B v->s, 46B s->v) and offer,
           then for a second display the video2 keys and offer, the same way
```

The Mac answers with rectangles of encoding 1010, a `u16` size and then:

- **message 1**, a 36-byte body: `u16` type, `u16` version, `u32` flags, then a
  `u16` port and `u32` flags for audio at `+8`/`+10`, video 1 at `+14`/`+16`,
  and video 2 at `+20`/`+22`, followed by ten reserved zero bytes. Bit 0 enables
  a leg. Apple's viewer requires it on audio and video 1, and takes video 2's as
  the Mac having two displays to send; remotex requires video 2 on exactly when the
  session asked for two. The measured ports were always
  5900 and 5901, the RFB port and the next, and 5902 for a second display. The viewer receives on the same
  numbers. The Mac sends message 1 once for the `SetEncodings` naming 1010, and
  again after each display change, never in reply to an offer.
- **message 2**, AVConference's answer: the common eight-byte header, `u16`
  lengths for the audio, video 1, and video 2 answer blobs, a zero `u32`, then
  those blobs. Remotex checks that their lengths describe the whole body and
  that video 2 has a blob exactly when it was offered. The answer sometimes comes twice for one offer.
  Apple's viewer disregards an answer that comes before message 1.
- **message 3**, a 16-byte error: the common header, then `u32` type and `u32`
  sub-code. Type 2 answers an offer the Mac could make no configuration
  from. Type 1 with sub-code 1 answers a `SetEncodings` naming 1010 from a
  second viewer: the Mac gives the stream to one viewer at a time, and ignores
  the display configurations of any other while it runs, so that viewer's
  layouts go on showing the first one's display. Remotex ends the session on
  either and names the other viewer for the second.

Each offer is a binary property list of four keys around a deflated
AVConference protobuf. Remotex rebuilds Apple's offers field by field and changes
one field, and a second in a session that passes the picture:

| Field | Apple's viewer | Remotex | Why |
|---|---|---|---|
| `0x1c` flags | 0 | `0x5` | Bit 2 makes the agent capture without the pointer (`send cursor with video 0`). Without it the pointer is drawn into every picture. Bit 0 is 60 fps, which the daemon sets anyway, with bit 1, for a message older than version 2; it does not bound the picture rate, the virtual display's refresh does. |
| `tilesPerFrame` (video stream field 6) | 4 | 4 decoded here or passed to the page's own decoder, 1 passed to the browser's or for a display under 144 rows that four tiles fail on | Four tiles split a frame into strips, each coded as a separate picture ([In strips](#in-strips)), which the gateway's decoder puts together, or the page's. One tile is one picture of the whole display on the one SSRC, which is what a browser's decoder is passed. |

The video offer names two codecs by their RTP payload numbers, 123 for H.264
and 100 for HEVC, each with its own feature string. Offered both, the Mac sends
HEVC.

Offered 123 alone, the Mac sends H.264 instead, as RTP payload type 123 on the
same leg and under the same keys, with the sound unchanged. On macOS 26.6 at
1600×1000 with one tile it was High profile, level 4.0, 8-bit 4:2:0, declaring 5
reference frames and a 13-bit picture order count, and tagged with the HEVC
stream's colours: Display P3 primaries, sRGB transfer, BT.709 matrix. Its
parameter sets do not come as NAL units of their own. A keyframe's first packet
is an MP4 `avc1` sample description holding them, and the IDR follows as FU-A
fragments. Remotex does not ask for it. Seen in one short run on the virtual Mac
only: no browser was given it, and a display change, a keyframe request and
more than one tile were not tried.

The flags are a big-endian `u32`, like the rest of the header: Apple's viewer
sets its bits and then byte-swaps the word before sending it. A published
description has the word in host order, which would move every bit to another
byte. Two other bits exist:
- bit 1 is bit 0 for the second video stream, which remotex sets beside it in an
  offer for two displays;
- bit 3 names Apple Remote Desktop, rather than Screen Sharing, as the video
  client, and remotex never sets it.

**The picture and the sound go together.** A configuration with an empty audio
offer is refused (`unable to create audio config`, error type 2), and one with an
empty video offer (`unable to create video config`, the same type). While the
audio leg runs, the Mac mutes its own sound output: the daemon's log shows the
output device muted as the stream starts, and the Mac's speakers were measured
silent. So an `ard-high-performance` session always carries sound and the
picker offers no choice of it. The stream takes over the Mac's sound, AirPlay included. Standard
mode never touches the sound output, so a Mac there plays to its speakers or to
an AirPlay receiver outside remotex as usual; in a High Performance session it
plays nothing to one, which was confirmed on a physical Mac.

**Ports first.** The Mac sends message 1 and the answer from separate paths, so
an offer sent before message 1 can be answered before it, and the Mac names its
ports once for the `SetEncodings` and once per display change, never again for an
offer made in its place. Remotex therefore offers as Apple's viewer does: once
for each message 1, and only once its display has settled.

**One offer at a time.** A second `0x1c` sent while the first one's capture was
still starting left the capture failed (`didStart: 0 error: 32000`). When the
virtual display was deallocated at the end of that session, WindowServer aborted
in `WSSelectiveSharingUpdateDisplayStreamSurface` and logged the console user
out. Remotex therefore has one offer out at a time, and sends no display change
while an offer is unanswered. An offer left unanswered ends the session with the
other failures (see [Liveness](#the-stream)).

### The stream

- **RTP.** Payload type 100, with a one-word header extension under profile
  `0x9311` or `0x9301` holding the picture's packet count and a frame counter.
  The profile is `0x9331` on a refresh picture (below), and that bit is all
  remotex reads of it. The marker bit ends a picture. RFC 7798 packetization:
  single NAL units, aggregation packets, fragmentation units. A stream of
  four tiles differs: see [In strips](#in-strips).
- **HEVC.** Range Extensions profile, 8-bit 4:4:4, full-range BT.709 matrix, sRGB
  transfer, Display P3 primaries, with wavefront parallel processing
  (`entropy_coding_sync_enabled_flag`) and no HEVC tiles, whatever
  `tilesPerFrame` is. libavcodec (FFmpeg 9.0.2,
  the prebuilt one configured down to the HEVC decoder) decodes it. Remotex
  gives the decoder four slice threads, which decode a picture's rows in
  parallel; frame threads would hold each picture back. On macOS the decoder
  is given a VideoToolbox device, and the archive's VideoToolbox hwaccel hands
  each picture to VideoToolbox. FFmpeg asks it to enable its hardware decoder
  for HEVC, not to require it, so nothing here checks that the media engine
  did the work. When the hwaccel fails to start, FFmpeg asks for a format
  again and the gateway takes a software one, falling back to the slice
  threads; a picture VideoToolbox fails once started is an error, as below.
  The log says which decoded it. A unit that fails to decode is an error, not
  a skipped picture, and brings a keyframe request.
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
  FIR brings an IDR within about 30 ms. Besides sender and receiver reports, the
  Mac accepts a compound packet that starts with PT 192, 193, 204, 205 or 206.
  - **The Mac drops a keyframe request made too soon.** It keeps the time of the
    last keyframe it made, which starts at the stream's own start, and discards
    a PLI or FIR that comes sooner after it than a least gap: 1 s for a stream
    of one tile, 10 ms for one of more. It logs
    `Request key frame too soon, discard` and sends nothing, and nothing later
    makes up for the request.
  - **AVConference's FIR** has two forms, chosen by a per-stream setting: RFC
    5104's (PT 206, FMT 4) and its own PT 192. The PT 192 form is the sender's
    SSRC and a list of 16-bit values, not RFC 2032's FIR, which is what a
    published description calls it.
  - **A loss can be mended without a keyframe.** The viewer acknowledges each
    picture it has whole, with an APP packet alone in its datagram whose name
    is the number 5 and whose four bytes are the picture's RTP timestamp, and
    the Mac's encoder keeps the newest acknowledged picture as a long-term
    reference. After a loss the viewer asks with payload-specific feedback of
    format 2 (PT 206) carrying the stream's width and height as two `u16`s
    after the two SSRCs. The Mac answers with a refresh picture: an ordinary
    picture predicted from the acknowledged one, marked by the `0x9331`
    profile, which a decoder that kept every acknowledged picture goes on
    from. The pictures between the loss and it predict from what was lost.
    Without an acknowledged picture the same request brings an IDR, as it did
    3 s into a stream that had been acknowledged throughout; that IDR, and the
    one a PT 192 request naming the size brings, came at about 27 KB where a
    PLI's and an RFC 5104 FIR's came at about 110 KB, on a 1600×1000 display
    the encoder had spent seconds refining. The request is subject to the
    least gap above. A PLI and a FIR bring an IDR whatever was acknowledged.
  - **Remotex acknowledges every picture of a stream of one tile that it
    hands on,** and after lost packets asks for a refresh and drops pictures
    until the marked one or an IDR. With 0.3% of the picture's packets dropped
    on the way to a gateway decoding the stream, the Mac's encoder logged three
    refreshes and no IDR after the stream's first, and no picture failed to
    decode. A browser decoding a passed stream across a refresh has not been
    watched.
  - **Remotex sends a PLI** where whoever is shown the stream has nothing left
    to predict from: when a stream starts without an IDR (the first packets
    can arrive before the socket is bound), when a picture fails to decode,
    when the decoder falls eight frames behind, which it warns about, and
    when a display comes back into view; for a passed stream, when the
    browser's link falls 15 behind and when the browser has to start over. It
    sends either request again every 500 ms until a picture it can go on from
    arrives, since the Mac may have dropped it and a still screen sends
    nothing else to show that. It also sends the rate reports described under
    [Rate control](#rate-control), every 50 ms on the picture's leg, as Apple's
    viewer does.
- **Liveness.** The `SetEncodings` naming 1010 owes message 1 within 10 s, and so
  does a display that has settled without one. Every offer owes its answer, its
  display's first picture and the first sound packet within 10 s, and the running
  stream an authentic
  packet, SRTP or SRTCP, on each leg every 48 s, 16 of Apple's 3-second
  timeouts. Apple's viewer times each leg from the last RTCP packet it
  received, not from pictures, which a still screen stops. The Mac's
  once-a-second reports keep both legs alive, and the sound leg also sends a
  packet every 10 ms whether or not anything plays. Past any of them, the
  session ends, as it does when the Mac refuses the offer (message 3)
  and when the receiver fails, on a socket error or an HEVC decoder
  that cannot start or stops. A display change stops the stream and owes nothing
  until its own offer, except the answer to an offer still out. When the Mac
  names its ports and nothing arrives within 5 s, the log names the port and the
  likely firewall or NAT.


#### In strips

Offered `tilesPerFrame` 4, as Apple's viewer offers it, the Mac answers 4 and
sends the display in four strips. Seen on the virtual Mac, at sizes from
160×120 to 1920×1080. The count the Mac answers is the lesser of its own and
the offer's, whatever the display's size.

- **The strips.** Each is the display's whole width and a quarter of its
  height rounded up to a multiple of 16: 208, 240, 256 and 272 rows for
  displays of 800, 900, 1000 and 1080. They lie top to bottom a strip's height apart, so the last runs
  past the display's last row, and what it holds there is not picture. The
  parameter sets give the strip's size and declare no cropping.
- **One SSRC a strip.** A strip's pictures come under the display's SSRC plus
  the strip's number, from 0, on the display's port and under its key, each
  SSRC with sequence numbers of its own, and so a rollover counter of its own.
  The strips of one frame share its timestamp and the header extension's frame
  counter, and the marker bit ends each strip's picture.
- **Some small displays get no picture.** Where the rounding leaves the last
  strip starting past the display's last row, the Mac answers the offer, its
  encoder fails every frame (`VCPCompressionSessionEncodeFrame failed`,
  error -12902), and no picture packet is sent: seen at 80, 90 and 136 rows,
  where an offer of one tile brought the picture. By that rule the heights are
  those under 48 rows, 65 to 95 and 129 to 143. A last strip that starts
  exactly at the end is sent, with nothing of the display in it: 96 and 144
  rows worked, as did 64, 100, 104, 112, 120 and 128. Remotex offers one tile
  for a display of those heights.
- **Only what changed.** A frame carries the strips that changed and no
  others. Under a moving pointer and a clock, 112 of 648 frames carried all
  four. Nothing in a packet says how many strips a frame has.
- **One HEVC stream.** The strips share one set of parameter sets and one
  decoding order.
  Every packet carries a 16-bit decoding order number, which counts each
  strip's picture across all four SSRCs without a gap, and a picture's order
  count is its place in that order. Merged in that order the pictures decode
  as one stream of strip-sized pictures, which is how remotex decodes them: one
  decoder, each picture placed by the SSRC it came under.
- **Strips predict from each other at a keyframe.** A keyframe is an IDR of
  strip 0 followed by an intra picture of each other strip, not an IDR, at
  the same timestamp. After it a strip's pictures name two earlier ones of the
  same strip, but among the first eight pictures of a stream nine references
  named another strip's. Strip 1 decoded on its own carried a grey band from
  its second picture to the end of a 15 s capture that decoded clean as one
  stream. Four decoders, one a strip, do not decode it.
- **The numbers are not laid out as RFC 7798 says.** An aggregation packet has
  one `DONL`, after the payload header, and no `DOND` between its units. Every
  fragment of a fragmented unit has the `DONL` after its FU header, not the
  first fragment alone.
- **No refresh pictures.** The parameter sets declare no long-term reference
  pictures (`long_term_ref_pics_present_flag` 0), which is what a refresh
  predicts from in a stream of one tile. A PLI sent as SRTCP brought the
  keyframe above in 28 ms; one sent in the clear, in another run, brought
  nothing.
- **Remotex takes any gap as the loss of everything since the last keyframe.**
  A packet missing from one strip's sequence, or a picture missing from the
  decoding order, drops every strip's pictures until strip 0's next IDR, and a
  PLI asks for it. It sends no acknowledgement and asks for no refresh.
- **The display is shown a frame at a time.** The gateway holds a frame's
  strips until it has all four for one timestamp, a strip of the next frame
  arrives, or 8 ms pass with no strip: the Mac codes a frame's strips one after
  another, and they arrived up to 7 ms apart.
- **A passed stream is in strips for the page's own decoder alone.** A
  browser's `VideoDecoder` shows each picture it decodes as the display, so a
  session passed to it is offered one tile. One started with *Decode HEVC in
  this page* is offered four: each strip goes out as a unit of its own, in the
  decoding order, under the display's size, with its number and whether it is
  the last of its frame, which the gateway knows as it knows when to show a
  frame it decodes, and the page's decoder puts the display together and shows
  it at each frame's last strip
  ([Apple's media stream, passed through](architecture.md#apples-media-stream-passed-through)).

### Rate control

This is High Performance's automatic media-stream control, not the **Adaptive**
quality choice in Standard mode. Every measured High Performance video
configuration, including Apple's viewer's, enables rate adaptation; its audio
configuration does not. The viewer offers no setting for it.

The Mac's encoder follows a rate controller on the Mac that works from the
viewer's reports alone. It moves between a floor of 20 Mbit/s and a ceiling of
the offer's bitrate entries, capped at 60 Mbit/s. The floor and the 60 Mbit/s
ceiling are fixed by the Mac's screen-sharing video profile, and no offer field
is known to lower the floor. The daemon logs the controller's state every 5 s:
target, cap, measured bitrate, round-trip time, one-way delay and loss.

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
- **The receiver's delay.** Apple's receiver takes one sample per picture, from
  its first packet: the lag is the packet's arrival less its RTP timestamp at
  24 kHz, both counted from the stream's first picture, so no clock is shared
  with the Mac. A short average (0.9 of itself, 0.1 of the lag) follows the lag
  and a long one (0.9999 and 0.0001) settles on its floor; the delay reported is
  the short less the long, and when that goes negative the long one takes the
  short one's value and the delay is 0. A lag more than 30 s from either average
  starts the estimate again. It is the delay of a queue building between the
  sender and the receiver's socket, and nothing after the socket enters it.
- **Below the floor.** An offer capped under 20 Mbit/s pins the controller at
  its floor, and the encoder runs at the cap whatever is reported: an animating
  lock screen came at about 7 Mbit/s under an 8 Mbit/s cap.
- **Remotex.** It offers Apple's entries unchanged and reports as Apple's viewer
  does: `RCTL` every 50 ms once a picture packet has arrived, the delay
  estimated as above from when the receiver reads each picture packet, loss 0
  and a bandwidth estimate of 60000 kbit/s, and the count and the delay starting
  again with each stream's SSRC. A receiver too busy to read its socket shows as
  delay too, which lowers the rate as a slow link would. It is the same for a decoded session
  and a passed one: what the browser's link does, the VP9 walk and the passed
  stream's queue answer, not the Mac's controller.
- **Apple's viewer.** It offers up to 100 Mbit/s and four tiles, sends `RCTL`
  every 50 ms, and acknowledges each decoded tile picture for the encoder's
  long-term references ([The stream](#the-stream)). On a quiet link its target sat
  at 58.4 Mbit/s with a round-trip time of about 1 ms. Its session was encrypted,
  so its reports were not read: the layout above comes from AVConference's code
  that builds and parses them, confirmed by a probe whose reports the Mac took as
  it takes the viewer's. This happens whenever High Performance runs; it is not
  selected by a quality control.

These are the virtual Mac's measurements, with synthetic reports. A congested
link to a physical Mac has not been observed.

### The sound

- **Codec.** AAC-ELD (MPEG-4 object type 39), whatever the offer lists: an
  offer with AAC-ELD removed was agreed and streamed AAC-ELD anyway. 48 kHz
  stereo, one 480-sample access unit per RTP packet (10 ms), payload type 101,
  about 320 kbit/s. The decoder is configured out of band with
  AudioSpecificConfig `F8 E6 50 00`: object type 39, 48 kHz, stereo, 480-sample
  frames, no SBR, no resilience tools.
- **What the offer decides.**
  - **The payloads.** The codec list does not choose the payload; field 4 of the
    offer's audio stream does. That field is a bitmask of the RTP payload types
    the viewer takes, one bit each. `0x1000` is 101, and the Mac's screen-sharing
    sound prefers 101. Apple's viewer sends `0x5E7F` (24191), and so does remotex.
  - **Not the rate.** The rate is the Mac's own: its screen-sharing sound
    configuration sets 320,000 bit/s whatever the offer says.
  - **A published description** reads field 4 as a bitrate the Mac picks a tier
    from. It is not one.
- **Decoder.** The gateway does not decode AAC-ELD: it passes every unit as it
  came, in every session, and the browser decodes it (`src/aac_eld.rs` holds
  the configuration it is told).
  Chrome 154's WebCodecs on macOS decoded all 3450 units of a capture, but only as
  `mp4a.40.2` with the AudioSpecificConfig above as the description; it refused
  `mp4a.40.39` as an unknown codec name. Safari 26.6 refuses `mp4a.40.39` too,
  since it is not on WebKit's WebCodecs allow-list. Safari's `isConfigSupported`
  says yes to Chrome's configuration, then fails every unit with
  "InternalAudioDecoderCocoa decoding failed". Safari reads the configuration
  through CoreAudio's `kAudioFormatProperty_FormatInfo`, which refuses a bare
  AudioSpecificConfig. Refused, Safari builds an AAC-LC decoder with no
  configuration, and the unified log shows that decoder rejecting each packet as
  `'bada'`. Given the same AudioSpecificConfig inside an MPEG-4 ES_Descriptor
  (`03 18 00 01 00 04 13 40 15 00 18 00`, eight zero bytes, `05 04 F8 E6 50 00`),
  `FormatInfo` reads `aace`, 48 kHz, stereo, 480 frames per packet, and Safari
  decoded all 3450 units. Chrome 153 on Windows also decoded every unit given the
  bare AudioSpecificConfig, and refused the ES_Descriptor as an unsupported
  configuration, so the two browsers need different descriptions. Both browsers'
  `isConfigSupported` say yes to both descriptions, the one each cannot decode
  included. Firefox 153 on Linux says yes to the bare AudioSpecificConfig and
  then produces no sound from it, without an error, and fails the ES_Descriptor
  with an `EncodingError`: it plays a session without sound.
  FFmpeg's native `aac` (libavcodec 62.28) decoded the capture cleanly at the
  same levels as Chrome and Safari.
- **Onward.** Each unit goes to the session's audio bridge as it came, and from
  there on `/ws/audio` to a browser that has the socket open, behind an
  `audioFormat` naming `mp4a.40.39` and the AudioSpecificConfig. A muted
  browser has no socket open, and nothing is sent or decoded for it.
- **Authentication.** Every packet is authenticated with its leg's own
  server-to-viewer key before it is decrypted, and the reports that keep the leg
  alive go out as SRTCP, as on the picture's leg. v0.0.249, which also decoded
  this sound, stripped the tag unread and sent plain RTCP.
- **Display changes.** A change stops the sound with the picture, and the next
  offer restarts both under new SSRCs on the same ports. The receiver carries
  on across it.
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
screen unasked, which is stepped over like every rectangle of such a session.

Once the first layout has arrived, a `SetEncodings` lists the media stream
first and is held for the rest of the session, as Apple's viewer lists it for
the whole of a High Performance session. The Mac's
preferred codec is the first it knows in the list, and its framebuffer sender
sends no pixels to a viewer whose preferred codec is the media stream: cursor
shapes, layouts and message 1 still come. That keeps the Mac's two framing
threads apart. Its sender frames updates under a lock, and the thread that reads
the viewer's messages frames the answer to an offer without it; a record from
each at once fails the record layer's integrity check and ends the session, the
answer carrying the trailer of the record before it. `SetEncodings` is acted on
under that lock, so an update being written is out before the first offer is
read, and none follows it or any later offer, across display changes too.
From that list on remotex asks for and arms one pixel, so the Mac is left
holding no request for a display that may have shrunk,
and a resize goes out as it falls due: the update a resize otherwise waits for
never comes from a Mac that sends no pixels.
A cursor change can still meet the answer, as it can for Apple's viewer.

The Mac counts the media stream as a codec only in its Apple silicon build: the
Intel slice of the same daemon never prefers it. A record that fails its check
is reported with its number, its size, and whether its trailer is a neighbouring
record's, which is what a number drawn twice leaves.

This also ends the input freeze behind a playing video. With the gateway capped at
15% of a core, the Mac's receive queue of our input was empty in 64 of 68
one-second samples and never above 3.5 KB. Over zlib under the same cap, input
went unread for 5–19 s at a time
([Other messages](#other-messages)).

### Display changes

Every display change stops both legs, so the sound drops out with the picture
until the new stream starts. The Mac then re-sends message 1 on its own,
with no stream behind it. The offer it allows starts a new stream on the same
ports, under a new SSRC, with an IDR at the new size. Remotex offers once message
1 has come and the display has settled. The resize's cover comes down when the
display settles, and `screenUnavailable` holds the page at "Screen not
available", with no input sent to the Mac, until that IDR is on its way to
the browser. Any rectangle the Mac sends meanwhile is stepped over.

### Reaching the gateway

The Mac sends from its own address to the viewer's address on the TCP connection,
from each port it named to the same port number at the viewer, so a NAT between
them has to pass it. The viewer's reports go out from the same
ports every second, which opens a port-preserving NAT's mapping. Every Mac uses the
same port numbers, so remotex binds them with address and port reuse and connects
each socket to its Mac. Several gateways on one host can then share the numbers,
unless one of them bound without reuse, as v0.0.249 did. Nothing arriving within
5 s of message 1 is logged, and the session ends when the offer's first picture
is 10 s overdue.

## Still unknown

- **`0x3f3`'s DCT tiles:** how their coefficients, and a partial update's
  refinements, are coded
  ([Apple's own framebuffer encodings](#apples-own-framebuffer-encodings)).
- **Other login types:** type 35's Kerberos tokens
  ([Other login types](#other-login-types)).
- **Rate control's loose ends**: the second byte of `RCTL`, whether loss lowers
  the target over longer than 30 s, and whether any offer field lowers the
  20 Mbit/s floor ([Rate control](#rate-control)).
- **Four-tile frames**: what Apple's viewer acknowledges a tile picture with,
  and whether anything short of a keyframe mends a loss; a stream of them
  under loss, on a physical Mac and at 4K
  ([In strips](#in-strips)).
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
