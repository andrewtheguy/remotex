# The RDP client against the specifications, audited

An audit of `src/rdp_client/` against the Microsoft Open Specifications kept in
[andrewtheguy/ms-rdp-specs](https://github.com/andrewtheguy/ms-rdp-specs), made on
2026-09-13. Every PDU the client encodes or decodes was compared field by field with
the spec text — MS-RDPBCGR of 2026-03-09, MS-RDPEGFX of 2026-05-11, and the
2024-04-23 revisions of MS-CSSP, MS-RDPEDYC, MS-RDPEDISP, MS-RDPECLIP, MS-RDPEFS,
MS-RDPEA, MS-RDPNSC, MS-RDPRFX and MS-RDPEGDI — with FreeRDP's `libfreerdp` as the
cross-check for what a real host does.

The target is a current Windows host and only that, as
[The RDP client](rdp-client.md) says, so every point is rated by its effect on
such a host. Points that matter only to legacy or non-Windows servers are notes.
Deliberate deviations already recorded in `rdp-client.md` or
[the roadmap](roadmap.md) are listed as confirmed, not as findings.

**Verdict:** the client puts no value on the wire the specifications forbid, reads
no field with the wrong bits, and honors every client rule the audit compared. What
remains is three questions only a real host can settle, the deviations the docs
already record, and the places the client is more lenient than a server's
constraints require.

## Method

The PDFs were converted with `pdftotext -layout` and grepped. Four reviews ran in
parallel — the connection sequence, the share and fast-path layer, the virtual
channels, the graphics pipeline — each reading its files in full and quoting the
spec section it compared against. The coordinator re-verified every point rated
above a note against the spec text and, where a reviewer cited FreeRDP, against
FreeRDP's source.

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
- The cookie's user name written as UTF-8 where MS-RDPBCGR 2.2.1.1 calls the field
  ANSI, cut at nine bytes as footnote 44 records of Microsoft's clients
  (`proto/x224.rs`).

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
  Ultimatum's straddled reason, and the confirms' four-bit result straddling the
  CHOICE byte as 4.1.7 annotates it; the Connect-Response `userData` length ignored
  as 3.2.5.3.4 requires; PER lengths written in X.691's fourteen bits.
- **GCC blocks:** `TS_UD_CS_CORE` at 234 bytes with every dependent-presence rule,
  `TS_UD_CS_SEC` naming the four methods of example 4.1.3, `TS_UD_CS_NET`, no
  extended blocks; the T.124 result read as its three-bit index; `TS_UD_SC_CORE`'s
  `clientRequestedProtocols` compared with the negotiation (3.2.5.3.4);
  `TS_UD_SC_SEC1` required to be method 0 and level 0 under Enhanced Security;
  `SC_NET` pad and count.
- **Security:** no Security Exchange PDU; Basic Security Header only on Client Info
  and licensing; `SEC_LICENSE_ENCRYPT_CS` tolerated; no `SEC_ENCRYPT`; rustls TLS
  1.2 and 1.3 with resumption disabled as MS-CSSP 3.1.5 requires.
- **Client Info:** every length and terminator rule, `CodePage` carrying the
  layout's language identifier under `INFO_UNICODE`, `clientAddressFamily`,
  `TS_TIME_ZONE_INFORMATION`, the legal truncation after `performanceFlags`,
  `INFO_NOAUDIOPLAYBACK` toggled by the target.
- **CredSSP:** version 6, 32-byte nonce, the SHA-256 binding hash in the specified
  order with its NUL, the public key as the `SubjectPublicKey` BIT STRING contents,
  the final `authInfo` sent without awaiting a reply.
- **Capabilities:** Confirm Active header with `originatorId 0x03EA`; all eleven
  mandatory sets plus Large Pointer and Multifragment at their spec lengths; every
  MUST value in General, Bitmap, Order, Input and Pointer; `VCChunkSize` of 1600;
  `MaxRequestSize` never below the 608,299 bytes `LARGE_POINTER_FLAG_384x384`
  requires; Demand Active iteration with unknown sets skipped; `VCChunkSize` range
  enforced on the server's set; the server's `inputFlags` required to admit
  fast-path input, and gating the horizontal wheel (2.2.8.1.1.3.1.1.3) and the
  extended mouse event.
- **Finalization and share layer:** Synchronize addressed to the Demand Active's
  `pduSource` (3.2.5.3.14), Control Cooperate then Request Control, Font List;
  server Synchronize, Cooperate, Granted, Font Map; share control and data headers
  including `uncompressedLength`; compressed bodies refused; Deactivate All to
  reactivation with no input in the window; Set Error Info fatal; Refresh Rect and
  Suppress Output sent only when advertised.
- **Fast-path input and output:** header, both length forms, event header packing,
  scancode flags, every mouse flag with the 9-bit rotation clamped, extended
  buttons; output action, encryption refused under TLS, the two-bit compression
  field with `FASTPATH_OUTPUT_COMPRESSION_USED` at `0x80`, fragmentation states and
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
- **Dynamic channels:** header packing, capabilities response at the server's
  version capped at 2, since 3 promises the compressed data this client refuses
  (MS-RDPEDYC 3.2.3.1), Create Response with echoed id and HRESULT, Data at most
  1590 bytes, Data First `Len` 3 refused, Close echoed for held channels and ignored
  for unknown ones, Soft-Sync correctly never negotiated.
- **Display Control:** header, `MonitorLayoutSize 40`, one primary at the origin,
  width even and both dimensions in 200 to 8192, scale factors in range or zeroed
  together, never sent before the server's capabilities.
- **Clipboard:** header with `dataLen` excluding it, capabilities of version 2 and
  flags 0, bytes past `dataLen` ignored per footnote 1, Monitor Ready then
  capabilities then Format List with early lists held, zeroed short names, one
  response flag per PDU, every Format Data Request answered.
- **Device redirection:** Announce Reply with the version minimum, Client Name
  Request with `CodePage 0` and the terminator counted, Core Capability Response
  with the general set's fields and `SpecialTypeDeviceCap` for minor 0x0C and up,
  and an empty Client Device List Announce after Client ID Confirm (MS-RDPEFS
  3.1.3).
- **Audio:** prolog, client formats as a subset of the server's, `wVersion 8`,
  Quality Mode only when both sides reach 6, Training Confirm echo, WaveInfo and
  Wave reassembly with the four carried bytes, Wave2, `wFormatNo` indexing the
  client's list, confirms on the receiving transport with `wTimeStamp` advanced by
  the milliseconds from arrival to sending (3.2.5.2.1.6), `WAVEFORMATEX` layout.
- **Graphics pipeline:** every server-to-client PDU's field order, ResetGraphics
  with 20-byte monitor definitions and the pad read past, CapsAdvertise with
  distinct versions and valid flags, FrameAcknowledge after compositing with a
  running `totalFramesDecoded`, cache kept across ResetGraphics.
- **ZGFX:** descriptors, segment sizes, the full literal and match token table,
  distance-zero unencoded runs with byte realignment, the 2,500,000-byte shared
  history, multipart totals, the trailer's five reserved bits masked (3.1.9.1.2.4).
- **ClearCodec:** flags, sequence increment, glyph index bound, composite header,
  run-length escapes, a residual layer allowed to stop short of its rectangle
  (2.2.4.1.1.1), band header with inclusive bounds and the 52-row limit, all
  three V-Bar forms with their exact bit layouts, cursor advance and wrap, cache
  reset, raw and RLEX subcodecs with the `numBits` formula, NSCodec as subcodec 1.
- **NSCodec:** header, `ColorLossLevel` range, plane sizes with subsampling,
  RLE, chroma recovery and the inverse YCoCg matrix, 2×2 supersampling.
- **Progressive:** every block layout, the sync block's magic and version ignored
  (2.2.4.2.1.1), `tileSize 64`, region header, both quant structures with
  `RFX_COMPONENT_CODEC_QUANT`'s band order, all three tile block kinds, tiles
  positioned on the surface, the
  reduce-extrapolate band sizes, LL3 delta and dequantization shifts, original and
  difference tiles, upgrade bit counts, SRL and RLGR1 constants and update rules,
  inverse DWT order, and the rule that a region is painted from every tile decoded
  since the frame began, which 2.2.4.2.1.5 specifies rather than merely permits.
