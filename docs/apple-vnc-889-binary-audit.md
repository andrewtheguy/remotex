# Apple RFB 003.889, as read from the binaries

An audit of remotex's Apple Screen Sharing client and of
[Apple RFB 003.889, as measured](apple-vnc-889.md), made on 2026-09-22 against the
x86-64 slices of the binaries a macOS 26 guest ships (signed August 2026):

- `ScreenSharing.framework`, the viewer's RFB client (Apple's own viewer);
- `screensharingd`, the server daemon, which is stripped — its functions are cited
  by Ghidra address, with the name its own `__func__` log strings give in brackets;
- `ScreensharingAgent`, the per-session agent that captures, posts input, owns the
  pasteboard and builds the display messages;
- `Screen Sharing.app`.

The measured document records what a Mac put on the wire. This one records what the
code that puts it there says, and where the two disagree the binary wins: several of
the measured corrections turn out to be one framing error seen from different
angles. A claim that only exists as a measurement (timings, accepted sizes, the
codec table, which lives in AVConference and was not extracted) is not contradicted
by its absence here.

## Method

Ghidra 12.1.4 headless decompiled the four binaries, with inferred no-return turned
off (the misnamed stubs were marked no-return and cut decompiles short); code Ghidra
had not disassembled was read with objdump. Four reviews ran in parallel —
handshake and framing, display configuration, input, clipboard and media — each
comparing the measured document and the code paths not already audited (the High
Performance resize sequencing and the media answer were) with the binaries. The
coordinator re-checked the findings that change code against the disassembly and
the gateway's own session logs. The binaries and decompiles are archived outside
the repository.

## Where remotex departs from Apple

Ordered by effect on a session.

### ClientInit asks for session selection

`ScreenSharing.framework` `_RFBAuthenticateCore` (`0x7ffa0ad1aca0`) builds the
ClientInit byte as `shared | 0x80` and adds `0x40` only when its caller supplies a
session-select handler. `screensharingd` `FUN_100038248` [HandleViewerInitialization]
answers `0x80` from an 888/889 viewer with the enhanced ServerInit, and `0x40` —
unless the `VNCSelectSession` preference is false — with a session-select exchange
whenever `FUN_10006592d` [SessionSelect_Needed] finds the authenticated user is not
the console user. ServerInit then carries flag `0x04` and a `0x4c`-byte block the
name length does not count. remotex sends `0xc1` and implements no session select,
so a High Performance connection as a user other than the one at the console reads
that block as a framebuffer update and fails before the record layer is up.

### The pasteboard is capped far below Apple's limit

The agent's pack (`FUN_10002e4c5`, CopyPackedScrapData) writes every saved flavor of
every item — RTF, HTML, web archive, TIFF, PDF — and both Apple ends cap an archive at
`0x6400000` (100 MB). remotex refuses an archive over `MAX_CLIPBOARD_BYTES × 4 +
64 KiB`, about 2.1 MB, whole, so a short text selection copied from Safari, Pages
or Preview arrives as oversized or unreadable. Its parser also treats the first
`u32` as an item count capped at 32 when it is the first item's flavor count, and
Office copies carry more.

An empty text sent from the browser becomes a promise on the Mac: the agent's
unpacker (`FUN_10002ed5d`) treats a flavor with `data_len 0` as "create promise" and
calls `PasteboardPutItemFlavor` with no data. A later paste asks the viewer for it
(MiscStatus command 3), and remotex answers with the same empty archive.

### The old display list is skipped with the wrong size

The agent's `FUN_10002802f` [EncodeDisplayInfoForDaemon] writes `DisplayInfo`
(`0x44d`) as `u16` width, `u16` height, `u32` flags, `u16` count, then `0x1c`-byte
records; Apple's viewer reads 10 bytes and the count from bytes 8–9. remotex reads
the count from the flags word. The daemon sends it only when `0x451` is not
advertised, so it is unreachable today.

### A display record remotex refuses and Apple keeps

- The agent's `hidpi_ScaleFactor` (`FUN_100045a47`) returns 0.0 on "bad mode ref";
  remotex drops a record outside 1–4, which for the single High Performance record
  ends the session.

## Corrections to the measured document

### The display layout

`ScreensharingAgent` `FUN_1000266f1` [EncodeDisplayInfo2ForDaemon] builds the
`0x451` layout as a standalone one-rectangle `FramebufferUpdate` and sends exactly
`count × 0x38 + 0x26` bytes of it. The rectangle's `u16` prefix is
`count × 0x38 + 0x14` and counts the bytes **after** itself:

| Payload offset (after the prefix) | Field |
|---|---|
| `+0x00` | `u16` version, 5 |
| `+0x02` | `u16 × 2` logical size of the union of screens |
| `+0x06` | `u16 × 2` backing size of the framebuffer |
| `+0x0a` | `u32` current display id, `0xffffffff` for combined |
| `+0x0e` | `u32` session state (see [the header word](#the-layouts-session-state-word)) |
| `+0x12` | `u16` display count |
| `+0x14` | records, `0x38` bytes each |

Apple's viewer (`HandleFramebufferUpdate`, case `0x451`, `0x7ffa0ad2e9b5` →
`0x7ffa0ad2f7b2`) reads the `u16`, `malloc(size)`, reads `size` bytes, takes the
count from `+0x12` and requires it to be 1–25 and `size ≥ 0x14 + count × 0x38`.

A record is `+0x00 f64` scale, `+0x08 f64` viewer scale (the daemon's server-side
scaling factor), `+0x10 u32` id, `+0x14` logical rect and `+0x1c` backing rect as
`(top, left, bottom, right)`, `+0x24 u32` flags, then the display's 16-byte pixel
format, whose last four bytes (blue shift and padding) are always zero.

The measured document's "two bytes shorter" and "fields two bytes later" were one
error: a reader that counted the prefix in its own length started the records two
bytes early and left those four zero bytes on the stream, where they parsed as an
empty `FramebufferUpdate` after every layout. In High Performance that phantom
update was a false boundary at which a queued `SetDisplayConfiguration` could go out
while the layout's full-size request was still outstanding.

The agent (`FUN_100027c2d`) sets record flag bit 1 whenever
`CGDisplayIsInMirrorSet` is true, which is every member of a mirror set, the one the
others copy included; `CGGetActiveDisplayList` lists only that one under hardware
mirroring. Apple's viewer builds a screen for every record.

### ServerInit's flags

`FUN_100038248` [SendServerInitialiation] writes a `u16` zero (the viewer checks the
whole word), a `u32` flag word, a 16-byte capability bitmap and the name:

| Bit | Meaning |
|---|---|
| `0x01` | Observe only: capture or event posting is not permitted. Apple's `_RFBSetMode` refuses control. |
| `0x02` | The user holds the Remote Management control privilege. |
| `0x04` | A session-select block follows. |
| `0x08` | Screen capture is not permitted; Apple's viewer aborts with "server is unable to read the screen". |
| `0x10` | Always set in the enhanced ServerInit: the maximum display count is present. |
| bits 5+ | `maximumVirtualDisplays` (`FUN_10005338f`, default 2). |

The measured `0x52` is `0x10 | 0x02 | 2 << 5`.

### `SetEncodings` is not order-sensitive for displays

The `SetEncodings` handler (`FUN_100038248`, case 2) resets the display flags on
every message, sets one for `0x44d` and one for `0x451`, and schedules a display-info
send for either. Order and duplicates have no effect on them; the only
order-sensitive state is the preferred codec, the first of 6, 16, 1000, 1001, 1002
and 1011 in the list. LastRect (−224) is recognised nowhere. `FUN_100026f0a`
[SendResolutionChargeToViewer] sends display info only when `0x44d` was listed, and
`DesktopSize` only when it was not and −223 was; `FUN_10001d351` [EncodeDisplayInfo]
sends the layout when `0x451` was listed and `DisplayInfo` otherwise. Every
`SetEncodings` carrying `0x44d` therefore produces another layout.

### There are no bare metadata messages

Apple's viewer `_HandleServerMessage` (`0x7ffa0ac16148`) handles server message types
0–3, `0x14`, `0x15`, `0x1e`, `0x1f`, `0x20`, `0x22`, `0x23` and `0x51`; anything else
is "error - unknown CMD" and closes the connection. `screensharingd` has no emitter for
bare `0x04`, `0x07` or `0x51`–`0x56`: every metadata item goes as a single-rectangle
`FramebufferUpdate` through `FUN_10001e7a4`. A reader two bytes out of step inside
`00 00 00 01 | 0 × 8 | 00 00 04 5x | u16 | payload` sees exactly what the measured
document describes — empty updates, then `04`, then `5x` and a length — which is what
the layout reader produced. A genuine bare `0x51` is `HandleServerSystemInfoData`: a
`u32` message size at `+2`, not a `u16` length.

`MiscStatus` is `14 00 00 04 00 01 <u16 command>` (`FUN_1000092e9`), sent only when
the viewer's `ViewerInfo` bitmap includes `0x14`, except `EncodeUserSessionChanged`
(`FUN_100022ac9`, command `0x11`), which is ungated. Command 2 is "server pasteboard
changed" and 3 "server pasteboard needs data".

### `ViewerInfo`

The daemon (case `0x21`) consumes `max(66, 4 + body_len)` bytes. Bytes 4–5 are the
message version and must be 1. The OS version triple is read: `FUN_1000451f0`
[ProcessKey] branches on major < 11 and minor < 15, so a viewer reporting 0.0.0 — or
sending no `ViewerInfo` — is handled as older than 10.15. The bitmap is the set of
server message types the viewer claims to handle; the daemon consults it for `0x14`
and `0x15` only.

### `SetEncryption`

`[2..4]` is the command, `[4..6]` an argument, `[6..8]` a method count, then `u32`
methods. Command 1 with method 1 generates a key and IV and schedules the rekey;
command 2 with argument 1 means "decrypt everything received from now". Apple's
viewer sends command 1 only when capability bit 18 is set, and command 2 after the
rekey arrives (`HandleEncryptionEncoding`, `0x7ffa0ad2e473`).

### The pointer mask

The daemon swaps mask bits 1 and 2 for every viewer except protocol 3.888 and
3.889 (`ReadViewerProtocolVersion`, `0x1000383d6`); the agent always reads the mask
positionally. The High Performance reading follows the protocol version rather than
the mode, and it is deliberate.

`HandleViewerCommand` (`0x10003a47c`), PointerEvent, after the swap at
`0x10003cdeb`:

```
10003ce12: cmpb $0x10, %al        ; exactly 0x10 → scroll down
10003ce1d: cmpl $0x8,  %ecx       ; exactly 0x08 → scroll up
10003ce20: jne  0x100042df7       ; anything else → PostMouseEvent
```

Only a mask of exactly `0x08` or `0x10` scrolls. The agent's
`PostMouseEventIntoSession` (`FUN_100024444`) reads every other mask positionally:
bit 0 left, bit 1 right, bits 2–7 `OtherMouseDown/Up` with the bit index as the
button number, so a wheel bit sent with a held button posts button 3 or 4 — Back
and Forward — and the horizontal `0x20`/`0x40` are clicks on buttons 5 and 6, which
also feed the agent's double-click chaining. The plain PointerEvent carries no
horizontal scroll at all; only the native `0x10` event (`FUN_100046336`) tests the
four wheel bits one by one. Each scroll pulse is
`CGEventCreateScrollWheelEvent(NULL, pixel, 2, …)` of one unit, and the release that
follows it, equal to the last posted state, is dropped by the agent as a repeat.

### Caps Lock

The agent's modifier merge (`FUN_100038e3a`) is `current & 0x942019 | required`,
where `required` is what the keyboard layout needs for the keysym: an uppercase
letter brings Shift with it, and a held Shift is stripped from a lowercase one. A
Command or Control shortcut under Caps Lock must therefore go out as the lowercase
keysym, or Command-Z arrives as Command-Shift-Z.

### Double-click

The agent chains clicks itself (`FUN_10002616a`): it resets the count when more time
than the threshold has passed since the last event with a button down, or when the
position differs at all, and increments it when more buttons are down than before.
The threshold is `NSEvent.doubleClickInterval × 10⁶`, read once in the agent's
`main()` (log line "doublick click time %u"). The measured document found a restart
did not change the live window, which puts the login-time caching in
`NSEvent.doubleClickInterval` rather than in the agent.

### `AutoFrameBufferUpdate`

The layout is `09 00 | u16 version 1 | u32 interval | x y w h`; a zero interval means
the server's own minimum and `0xffffffff` disarms. The daemon does have a push path:
`FUN_100023114` takes a rectangle from the damage list, clips it to the armed region
and sends it with no request pending, gated by `FUN_100027789`. The measured "does not
stream" stands as an observation of the tested sessions, not as the server's design.
Apple's viewer arms only the full framebuffer (a NULL rect).

### Keys the Mac drops

The agent's keysym tables have no entry for Insert, Pause, Scroll Lock, Print or
Menu; the Mac logs "unable to handle keysym". Num Lock maps to Keypad Clear. On the
plain key path the agent strips Option and Shift from every non-special key, so
Option+letter arrives as the plain character unless Command is also held.

### The display configuration descriptor

- Descriptor `+0x02` is the display's name: the daemon NUL-terminates it and the
  agent passes it as `initWithName:`.
- `display_flags` bit 1 is "do not adjust refresh rate"; the agent ignores
  `display_type` and sets type 4.
- Mode flags bit 0 is HDR reference.
- Apple's `_RFBSetDisplayConfiguration` overwrites the caller's maximum size with the
  largest mode it lists, so native never sends a maximum larger than its largest
  mode; remotex's single mode under a 3840×2160 maximum is a shape Apple's library
  cannot emit, measured to work. The agent applies the maximum, millimetres and name
  only when it first creates the display (`FUN_10002b827`).
- Unless the `com.apple.RemoteManagement BlankScreen` preference is false the agent
  creates the virtual display exclusive (option `0x40`), which is what hides the
  physical screens.

### The layout's session-state word

The header word the measured document calls unidentified is session state:
`0x04` on console, `0x01` obscured, `0x02` locked when obscured, `0x08` cannot be
modified, `0x10` login not done. remotex's own High Performance logs read 5.

### The media offer

`HandleAVCMediaStreamEncoding` (`0x7ffa0ad31120`) reads separate ports rather than
one base port: offsets from the rectangle body after its `u16`, flags `u32` at `+4`,
audio UDP `u16` at `+8`, audio flags at `+10`, video 1 UDP at `+14` and flags at
`+16`, video 2 UDP at `+20` and flags at `+22`. The viewer rejects message 1 unless
`audio_flags & video1_flags & 1`. The `u16` rectangle size does not count itself;
the viewer's minimums are 5 for any message, `0x23` for message 1, `0x11` for 2 and
`0xf` for 3. `_RFBMediaStreamServerConfiguration` (`0x7ffa0ad26a41`) leaves a zero
`u32` at `+0x10` of message `0x1c`. The selectors are
`defaultsForStreamGroupID:streamIndex:` and `isOpus4Channel48KhzPayload:outFormat:`.

## Confirmed

- Type-30 authentication: `u16` generator, `u16` key length, prime, public key
  (`FUN_100018b5b`); the 128-byte credential block is AES-ECB, username at 0 and
  password at 64 (`FUN_100013f78`).
- The rekey is a one-rectangle update with encoding `0x44f` and a `0x34`-byte body —
  `u32` generation (always 1), then key and IV, each ECB-wrapped — and the wrap key
  rotates to the new key. The record sequence counter is never reset.
- Records (`FUN_10005e9e7`): length `(b + 0x25) & ~15`, trailer
  `SHA1(be32 seq ‖ plaintext)`, filler repeating the last body byte, at most `0x8000`
  bytes per record.
- `0x453`, `0x455` and `0x456` each carry a `u16` counting what follows.
- `SetDisplayConfiguration` (`0x1d`) and `SetDisplay` (`0x0d`) match field for field;
  the daemon clamps the display count to 2 and requires at least `0xc0` bytes.
- `AutoPasteboard` (`0x15`), the clipboard fetch (`0x0b`), both directions of `0x1f`
  and the archive layout match; both ends deflate at level 9 with one
  `Z_SYNC_FLUSH`, and the agent rejects a stream that ends with `Z_STREAM_END`.
- Message lengths: KeyEvent 8, PointerEvent 6, FramebufferUpdateRequest 10,
  `AutoFrameBufferUpdate` 16. Incremental 0 is a forced full update.
- `SetMode` `0a 00 00 01` matches `_RFBSetMode`.

## Not settled by the binaries

- The negotiation codec table, the audio payload type and bitrate, and the RTCP
  timeout: AVConference was not extracted.
- What Apple's viewer puts in its descriptor (name, modes, rotations = 7): that is
  built in ScreenSharingUI, also not extracted.
- ClientInit `0x81` against a non-console user, a mirrored Mac, and whether the
  `AutoFrameBufferUpdate` push path ever fires, all need a live Mac.
