# The RDP client against the specifications, audited

An audit of `src/rdp_client/` against the Microsoft Open Specifications kept in
[andrewtheguy/ms-rdp-specs](https://github.com/andrewtheguy/ms-rdp-specs), made on
2026-09-13 at commit `1765ad4`. Every PDU the client encodes or decodes was compared
field by field with the spec text — MS-RDPBCGR of 2026-03-09, MS-RDPEGFX of
2026-05-11, and the 2024-04-23 revisions of MS-CSSP, MS-RDPEDYC, MS-RDPEDISP,
MS-RDPECLIP, MS-RDPEFS, MS-RDPEA, MS-RDPNSC, MS-RDPRFX and MS-RDPEGDI — with
FreeRDP's `libfreerdp` as the cross-check for what a real host does.

The target is a current Windows host and only that, as
[The RDP client](rdp-client.md) says, so every finding is rated by its effect on
such a host. Points that matter only to legacy or non-Windows servers are notes.
Deliberate deviations already recorded in `rdp-client.md` or
[the roadmap](roadmap.md) are listed at the end as confirmed, not as findings.

**Verdict:** nothing found breaks a session against a current Windows host. Eight
places put a value on the wire the spec forbids or decode a field with the wrong
bit, all tolerated by Windows; nine more skip a rule the client is bound by; one
codec state question needs a real host to settle.

## Method

The PDFs were converted with `pdftotext -layout` and grepped. Four reviews ran in
parallel — the connection sequence, the share and fast-path layer, the virtual
channels, the graphics pipeline — each reading its files in full and quoting the
spec section it compared against. The coordinator re-verified every finding rated
above a note against the spec text and, where a reviewer cited FreeRDP, against
FreeRDP's source; one such citation was wrong and is corrected below.

## Wire-level violations, tolerated by Windows

Each of these sends a value the spec forbids, or reads a field wrongly. None has
been seen to matter against a Windows host; each is a MUST the client breaks.

1. **No encryption method advertised.** `proto/gcc.rs:289` writes
   `encryptionMethods = 0` and `extEncryptionMethods = 0`. MS-RDPBCGR 2.2.1.3.3:
   "The client MUST specify at least one encryption method", and 3.3.5.3.3 tells a
   server that finds no valid flag to close the connection. Microsoft's own example
   in 4.1.3 sends `0x1B` under TLS. Cost of fixing: one constant.
2. **`CodePage` is zero with `INFO_UNICODE` set.** `proto/info.rs:119`.
   MS-RDPBCGR 2.2.1.11.1.1: "if the flags field contains the INFO_UNICODE flag, then
   the CodePage field MUST contain the active language identifier in the low-word".
   The comment above the line inverts the spec's rule — the server ignores the
   identifier when `keyboardLayout` is *zero*, and this client sends `0x0409`.
   Footnote 17 says Windows applies it to a newly created session.
3. **Fast-path compression flag tested on the wrong bit.** `proto/fastpath.rs:53`
   defines `COMPRESSION_USED` as `0x40`. In MS-RDPBCGR 2.2.9.1.2.1 the
   `compression` field is the top two bits of the update header with value 2, so
   the wire flag is `0x80`; FreeRDP reads `(header >> 6) & 3`. A conforming header
   carrying the flag is refused as "its compression 0x80" and the session ends.
   Latent, because the client never sets `INFO_COMPRESSION` and so a Windows host
   never sets the bit; the unit test at `fastpath.rs:381` encodes the same wrong
   constant.
4. **`VCChunkSize` of zero.** `proto/capabilities.rs:355` writes a 12-byte Virtual
   Channel capability set whose `VCChunkSize` is 0. MS-RDPBCGR 2.2.7.1.10: the value
   "MUST be greater than or equal to CHANNEL_CHUNK_LENGTH and less than or equal to
   16256". The server ignores the client's value, so 1600 there costs nothing.
5. **`LARGE_POINTER_FLAG_384x384` without the buffer it requires.**
   `proto/capabilities.rs:118` sets both large-pointer flags, and
   `connect.rs:261` echoes the server's `MaxRequestSize` unchecked. MS-RDPBCGR
   2.2.7.2.7: "If the LARGE_POINTER_FLAG_384x384 (0x00000002) flag is included,
   then the MaxRequestSize MUST be set to at least 608,299 bytes." The Confirm
   Active can therefore make two promises that contradict each other.
6. **DVC capabilities version echoed as 3.** `session.rs:1450` answers the
   server's Capabilities Request with the version the server asked for, which a
   Windows host makes 3. MS-RDPEDYC 3.2.3.1 reserves 3 for a client that "supports
   compressed dynamic virtual channel data as well as channel priorities";
   `proto/dvc.rs:197` refuses the compressed Data First and Data commands. Windows
   does not send them over TCP. FreeRDP echoes the same way.
7. **MCS `result` read as a whole byte.** `proto/mcs.rs:329`. In aligned PER the
   T.125 `Result` is a 4-bit field straddling two bytes, as MS-RDPBCGR 4.1.7
   annotates: its top bit is the low bit of the CHOICE byte, which `domain_pdu`
   discards. `rt-parameters-unacceptable (8)` therefore decodes as success and
   fails later with a misleading "short" error, and every other failure is reported
   shifted left by five. `gcc.rs:351` has the same shape for the T.124 result.
   FreeRDP shares the shortcut.
8. **PER length writer admits 15 bits.** `proto/per.rs:20` sets `MAX_LENGTH` to
   `0x7FFF` and `per.rs:34` writes `length | 0x8000`. X.691 10.9.3.7 gives the
   two-octet form 14 bits; `11xxxxxx` introduces a fragmented value. Unreachable
   today — the largest payload is a 1600-byte channel chunk — but the writer's
   contract admits it.

## Client rules not honored

Each is a rule on the client's own behavior, SHOULD or MUST, that the code does
not follow. Windows tolerates every one.

1. **Horizontal wheel sent unconditionally.** `proto/input.rs:58`. MS-RDPBCGR
   2.2.8.1.1.3.1.1.3, `PTRFLAGS_HWHEEL`: "This flag MUST NOT be sent to a server
   that does not indicate support for horizontal mouse wheel events in the Input
   Capability Set." `DemandActive::decode` never reads the server's `inputFlags`,
   so no gate could honor this, nor `INPUT_FLAG_FASTPATH_INPUT2` or
   `INPUT_FLAG_MOUSEX` before fast-path or extended-button events.
2. **Wave Confirm before playback, timestamp unadjusted.** `proto/rdpsnd.rs:242`
   builds the confirm in the turn that hands the samples to the sink, and
   `rdpsnd.rs:334` echoes `wTimeStamp` verbatim. MS-RDPEA 3.2.5.2.1.6: the field
   "MUST be set to the same field of the originating ... PDU, plus the time, in
   milliseconds, between receiving the complete wave PDU from the network and
   sending this PDU", and 2.2.3.8 wants the confirm after the sample "is emitted to
   completion". The host is told every block played with zero latency.
3. **No Client Device List Announce.** `proto/rdpdr.rs:109` ends the handshake at
   Client ID Confirm. MS-RDPEFS 3.1.3 step 4: "the client MUST send Client Core
   Capability Response (section 2.2.2.8) and Client Device List Announce Request
   (section 2.2.2.9)", and an empty list is legal. The comment's claim that FreeRDP
   does the same is *correct*: `rdpdr_send_device_list_announce_request` returns
   before sending when the count is zero. A reviewer said otherwise; it was
   checked.
4. **Synchronize `targetUser` is the client's own channel.**
   `proto/finalization.rs:96`. MS-RDPBCGR 3.2.5.3.14: "The targetUser field SHOULD
   be set to the MCS server channel ID that is held in the Server Channel ID store",
   which is the Demand Active's `pduSource`; `share.rs:149` discards it. Windows
   ignores the field.
5. **`clientRequestedProtocols` never compared.** `proto/gcc.rs:379` skips
   `TS_UD_SC_CORE` whole. MS-RDPBCGR 3.2.5.3.4: the field "is examined to ensure
   that it contains the same flags that the client sent ... If this is not the
   case, the client SHOULD drop the connection." This is the spec's anti-downgrade
   replay of the clear-text negotiation; CredSSP's key binding covers the same
   ground here.
6. **Connect-Response `userData` length trusted.** `proto/mcs.rs:139` bounds the
   GCC response by the BER length. MS-RDPBCGR 3.2.5.3.4: "The client MUST ignore
   the specified length of the MCS Connect Response PDU user data."
7. **Progressive sync magic and version refused.** `proto/progressive.rs:296`.
   MS-RDPEGFX 2.2.4.2.1.1 says of both: "The decoder SHOULD ignore this value." A
   future Windows build that bumps the wire version would have every Progressive
   PDU dropped. FreeRDP has the same check.
8. **ZGFX trailer byte not masked.** `proto/zgfx.rs:182` uses the whole final
   byte as the unused-bit count. MS-RDPEGFX 3.1.9.1.2.4: "some value between 0 and
   7, inclusive ... The five high-order bits in the last byte of the compressed
   segment are reserved." FreeRDP does the same.
9. **ClearCodec residual must fill the rectangle.** `proto/clear.rs:373` refuses a
   residual layer that stops short. MS-RDPEGFX 2.2.4.1.1.1: "The number of pixels
   encoded by this structure MUST be less than or equal to the number of pixels in
   the original image." FreeRDP refuses too.

## To settle against a host

These could not be decided from the text alone. The probe in
`tests/rdp_client_probe.rs` against the operator's `tmp/test_uat.toml` target is
the instrument; it was not run for this audit because it takes over the remote
clipboard.

- **The ClearCodec sequence counter across a graphics reset.** `proto/clear.rs:196`
  requires each rectangle to carry the number after the last, mod 256, and
  `gfx.rs:318` leaves the counter alone on ResetGraphics. The spec is silent about
  the reset. FreeRDP's `clear_context_reset`, called from `gdi_ResetGraphics`, sets
  its counter to zero and then accepts whatever first number arrives — which
  suggests a Windows host restarts the sequence after a resize. If it does, this
  client refuses up to 255 ClearCodec rectangles after every resize or density
  change, each a warning naming "a ClearCodec sequence number", until the counters
  realign. Run the probe with `egfx = true` and `resize = true` and read the log
  after the resize step:

  ```sh
  REMOTEX_UAT_TARGET=<rdp target> cargo test --test rdp_client_probe -- --ignored --nocapture --test-threads 1
  ```

- **Whether auto-detect PDUs arrive at all.** `rdp-client.md` says "The auto-detect
  PDUs a Windows host sends anyway go unanswered". The client requests no MCS
  message channel (`proto/gcc.rs:20`) and does not set
  `RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT`, and MS-RDPBCGR 2.2.14.3 says those PDUs
  "MUST only be sent over the MCS message channel", so a conforming host has
  nowhere to send them. One arriving on the I/O channel would be parsed by
  `share::decode` as a share control header and end the session on "its version".
  The doc's claim should be re-measured or reworded.
- **1-bpp pointer mask row order.** `proto/pointer.rs:257` reads a monochrome
  pointer's masks top-down, matching FreeRDP's `vFlip = (xorBpp == 1) ? FALSE :
  TRUE`. MS-RDPBCGR 2.2.9.1.1.4.4 calls both masks bottom-up with no exception for
  1 bpp. A monochrome cursor from a Windows host would decide it.

## Stale comments and citations

Behavior is right in each of these; the words beside it are not.

- `docs/rdp-client.md`, Sound: "Close clears the format." `proto/rdpsnd.rs:192`
  keeps it, deliberately and with a test, because a Windows host sends its format
  list once and a Close after every stream. The module header says so.
- `proto/info.rs:119`: the `CodePage` comment inverts the spec's rule; see above.
- `proto/x224.rs:56`: "[MS-RDPBCGR] 2.2.1.1 caps the whole cookie field at 28
  bytes." No such cap exists; the nine-character truncation is mstsc's behavior in
  footnote 44. The same function emits UTF-8 for a non-ASCII user name into a field
  the spec calls ANSI.
- `proto/x224.rs:152`: the `DYNVC_GFX` flag is documented as "This client decodes
  bitmaps and does not open it", which predates the pipeline.
- `proto/cliprdr.rs:34` and `rdp-client.md` cite MS-RDPECLIP 3.1.5.2 for the
  long-format-names rule. The normative text is 2.2.2.1.1.1: "If this flag is not
  set, the Short Format Name variant MUST be used." The conclusion holds.
- `src/rdp_clipboard.rs:78` attributes `CF_UNICODETEXT`'s NUL termination to
  MS-RDPECLIP, which says nothing about the payload; it is the Windows clipboard
  format's rule.
- `proto/progressive.rs:80` documents `Quant::read` as a `TS_RFX_CODEC_QUANT`. The
  nibble order implemented is `RFX_COMPONENT_CODEC_QUANT`'s, which MS-RDPEGFX
  2.2.4.2.1.5.2 says "differs from the TS_RFX_CODEC_QUANT ... with respect to the
  order of the bands". The code is right; the citation would mislead.
- `rdp-client.md` presents as measured that the client's own graphics PDUs go out
  unwrapped. MS-RDPEGFX 2.1 specifies it: "Client-to-server graphics messages are
  not encapsulated within any external structure."
- `proto/gcc.rs:107` says `CHANNEL_OPTION_SHOW_PROTOCOL` tells the server and the
  chunk flag "reminds" it. MS-RDPBCGR 2.2.1.3.4.1 says the option "MUST be ignored
  by the server"; only the per-chunk flag is normative. The wire behavior is
  consistent with the spec either way.

## Documented deviations, confirmed

Each of these is a deliberate departure the docs already record, checked here
against the spec text and found to be permitted or a SHOULD.

- `PROTOCOL_HYBRID` offered without `PROTOCOL_SSL` (MS-RDPBCGR 2.2.1.1.1, SHOULD).
- `RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL` set without network-detection support
  (2.2.1.3.2 says the former "requires" the latter); auto-detect unanswered.
- The TLS certificate chain unverified; the handshake signature verified and the
  public key bound by CredSSP. MS-CSSP 3.1.5 step 1 requires no trusted root. On
  [the roadmap](roadmap.md#verifying-the-servers-certificate-chain).
- Licensing accepts only `ERROR_ALERT` with `STATUS_VALID_CLIENT`; 3.2.5.3.12
  makes dropping anything else a MAY. On
  [the roadmap](roadmap.md#licensing-on-a-remote-desktop-session-host).
- `RDPGFX_CAPS_FLAG_THINCLIENT` unset. Under
  [Not planned](roadmap.md#thinclient-in-the-graphics-capability-advertise).
- Server pointer positions dropped (2.2.9.1.1.4.2 says the pointer MUST move).
- Inbound static-channel PDUs over 4 MiB dropped, not reassembled (3.1.5.2.2.1).
- ClearCodec band columns painted beside the destination rectangle, decoded over
  the surface's existing pixels, glyphs stored as the surface looks afterwards.
- The audio format kept across a Close PDU (MS-RDPEA 3.2.5.2.1.7 describes a
  restart).

## Leniencies toward a non-conforming server

Not violations — the constraint binds the server — but the client accepts what the
spec says it will not be sent, so a bad host produces a wrong picture rather than a
named refusal. Listed so nobody rediscovers them.

- ClearCodec: the first `seqNumber` may be nonzero; a glyph may be up to 1024×1024
  pixels where 2.2.4.1 caps it at 1024 pixels; a `VBAR_CACHE_HIT` entry's stored
  height is not checked against the band's.
- Bitmap cache slots: slot 0 and any `u16` slot accepted; the client advertised
  `SMALL_CACHE`, whose limit is 4096 one-based slots.
- Progressive: a REGION outside FRAME_BEGIN and FRAME_END is decoded (2.2.4.2.1.5
  says ignore it); `codecContextId` is ignored and state keyed by surface;
  difference tiles are accumulated in the dequantized domain, equivalent only while
  a tile's quant table holds between passes. FreeRDP is identical on each.
- ZGFX: three 9-bit match tokens beyond the spec's table are accepted, harmlessly,
  since any distance past the history is refused; the header check is
  `flags & 0x04 == 0` rather than `flags & 0x0F == 0x04`.
- NSCodec does not apply MS-RDPEGDI 3.1.9.1.2's B/R swap; MS-RDPNSC's worked
  example decodes correctly without it, and FreeRDP does not swap.
- Static channels: a chunk with neither `FIRST` nor `LAST` and no open sequence
  ends the session where 3.1.5.2.2 says it "can be dispatched"; `SUSPEND` and
  `RESUME` are ignored; a channel PDU arriving during licensing or before Demand
  Active aborts the connection, where `activate` defers one arriving later.
- Clipboard: a malformed Format List ends the session where MS-RDPECLIP 3.1.5.2.2
  wants a failing Format List Response; a long-name list whose length happens to be
  a multiple of 36 would parse as short names.
- Display Control: the monitor area cap is computed at `proto/display.rs:92` and
  never compared, unreachable under the 8192 clamp.
- Share layer: the T.128 `0x8000` flow marker is read as a length; a Server
  Redirection PDU is refused as "its type 0xa" rather than named; `dwStateTransition`
  in the License Error PDU is not checked; Client Info strings are capped at 64 KiB
  rather than the spec's 512 bytes each.
- Finalization refuses any share data PDU other than the four expected plus Set
  Error Info, Save Session Info and Monitor Layout; a Set Keyboard Indicators in
  that window would end the connection.

## Verified correct

Coverage, so a reader knows what was compared and found right rather than merely
not mentioned. Each line was checked field by field against the spec text.

- **X.224 and MCS:** TPKT and Data TPDU headers; Connection Request LI, cookie and
  `RDP_NEG_REQ`; Connection Confirm decode including the absent-negotiation case and
  all six failure codes; BER Connect-Initial with the 3.2.5.3.3 domain parameters;
  GCC Conference Create Request byte-identical to example 4.1.3; Erect Domain,
  Attach User, Channel Join, Send Data Request and Indication byte-identical to
  4.1.5 through 4.1.11; join order and channel-id checks; Disconnect Provider
  Ultimatum's straddled reason.
- **GCC blocks:** `TS_UD_CS_CORE` at 234 bytes with every dependent-presence rule,
  `TS_UD_CS_NET`, no extended blocks; `TS_UD_SC_SEC1` required to be method 0 and
  level 0 under Enhanced Security; `SC_NET` pad and count.
- **Security:** no Security Exchange PDU; Basic Security Header only on Client Info
  and licensing; `SEC_LICENSE_ENCRYPT_CS` tolerated; no `SEC_ENCRYPT`; rustls TLS
  1.2 and 1.3 with resumption disabled as MS-CSSP 3.1.5 requires.
- **Client Info:** every length and terminator rule, `clientAddressFamily`,
  `TS_TIME_ZONE_INFORMATION`, the legal truncation after `performanceFlags`,
  `INFO_NOAUDIOPLAYBACK` toggled by the target.
- **CredSSP:** version 6, 32-byte nonce, the SHA-256 binding hash in the specified
  order with its NUL, the public key as the `SubjectPublicKey` BIT STRING contents,
  the final `authInfo` sent without awaiting a reply.
- **Capabilities:** Confirm Active header with `originatorId 0x03EA`; all eleven
  mandatory sets plus Large Pointer and Multifragment at their spec lengths; every
  MUST value in General, Bitmap, Order, Input and Pointer; Demand Active iteration
  with unknown sets skipped; `VCChunkSize` range enforced on the server's set.
- **Finalization and share layer:** Synchronize, Control Cooperate then Request
  Control, Font List; server Synchronize, Cooperate, Granted, Font Map; share
  control and data headers including `uncompressedLength`; compressed bodies
  refused; Deactivate All to reactivation with no input in the window; Set Error
  Info fatal; Refresh Rect and Suppress Output sent only when advertised.
- **Fast-path input and output:** header, both length forms, event header packing,
  scancode flags, every mouse flag with the 9-bit rotation clamped, extended
  buttons; output action, encryption refused under TLS, fragmentation states and
  the 3.2.5.9.3.1 reassembly rules, all update codes.
- **Bitmap and pointer updates:** `TS_BITMAP_DATA` with and without the
  compression header, bottom-up uncompressed rows; every pointer attribute
  structure with masks in the specified order and 2-byte scan-line padding; the
  32-entry cache.
- **Planar:** format header bits, plane order, RLE control byte and the extended
  run encodings against the spec's examples, delta decode per 3.1.9.2.3, and the
  row flip difference between bitmap updates and the pipeline.
- **Static channels:** header length on every chunk, `FIRST` and `LAST` placement,
  chunk size from the server's set, MCS priority and segmentation bits, reassembly
  consistency checks, compression refused consistent with `VCCAPS_NO_COMPR`.
- **Dynamic channels:** header packing, capabilities response, Create Response with
  echoed id and HRESULT, Data at most 1590 bytes, Data First `Len` 3 refused,
  Close echoed for held channels and ignored for unknown ones, Soft-Sync correctly
  never negotiated.
- **Display Control:** header, `MonitorLayoutSize 40`, one primary at the origin,
  width even and both dimensions in 200 to 8192, scale factors in range or zeroed
  together, never sent before the server's capabilities.
- **Clipboard:** header with `dataLen` excluding it, capabilities of version 2 and
  flags 0, bytes past `dataLen` ignored per footnote 1, Monitor Ready then
  capabilities then Format List with early lists held, zeroed short names, one
  response flag per PDU, every Format Data Request answered.
- **Device redirection:** Announce Reply with the version minimum, Client Name
  Request with `CodePage 0` and the terminator counted, Core Capability Response
  with the general set's fields and `SpecialTypeDeviceCap` for minor 0x0C and up.
- **Audio:** prolog, client formats as a subset of the server's, `wVersion 8`,
  Quality Mode only when both sides reach 6, Training Confirm echo, WaveInfo and
  Wave reassembly with the four carried bytes, Wave2, `wFormatNo` indexing the
  client's list, confirms on the receiving transport, `WAVEFORMATEX` layout.
- **Graphics pipeline:** every server-to-client PDU's field order, ResetGraphics
  with 20-byte monitor definitions and the pad read past, CapsAdvertise with
  distinct versions and valid flags, FrameAcknowledge after compositing with a
  running `totalFramesDecoded`, cache kept across ResetGraphics.
- **ZGFX:** descriptors, segment sizes, the full literal and match token table,
  distance-zero unencoded runs with byte realignment, the 2,500,000-byte shared
  history, multipart totals.
- **ClearCodec:** flags, sequence increment, glyph index bound, composite header,
  run-length escapes, band header with inclusive bounds and the 52-row limit, all
  three V-Bar forms with their exact bit layouts, cursor advance and wrap, cache
  reset, raw and RLEX subcodecs with the `numBits` formula, NSCodec as subcodec 1.
- **NSCodec:** header, `ColorLossLevel` range, plane sizes with subsampling,
  RLE, chroma recovery and the inverse YCoCg matrix, 2×2 supersampling.
- **Progressive:** every block layout, `tileSize 64`, region header, both quant
  structures, all three tile block kinds, tiles positioned on the surface, the
  reduce-extrapolate band sizes, LL3 delta and dequantization shifts, original and
  difference tiles, upgrade bit counts, SRL and RLGR1 constants and update rules,
  inverse DWT order, and the rule that a region is painted from every tile decoded
  since the frame began, which 2.2.4.2.1.5 specifies rather than merely permits.
