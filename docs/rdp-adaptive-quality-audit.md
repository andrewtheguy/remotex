# RDP adaptive-quality audit

This is the one work list for the Windows RDP adaptive-quality investigation.
Add every confirmed defect found during this work here, including defects in the
probe that are not themselves the cause of adaptive quality. Check an item only
after its acceptance criteria pass.

The first gate is always the link between the gateway and the Windows host. Do
not promote network auto-detection into ordinary sessions, implement a browser
path, or start browser QA until the open implementation defects below are fixed
and the gateway-to-host comparison is valid.

## What the current measurements establish

The real-host runs in `tmp/autodetect/lan.log` and
`tmp/autodetect/throttle1000.log` asked for whole-desktop AVC444v2 and answered
the host's network auto-detection requests.

| Run, after the first 5 seconds | Frames/s | Total rate | Bytes/frame | AVC444v2 rate | H.264 QPs |
| --- | ---: | ---: | ---: | ---: | --- |
| LAN | 31.8 | 2,427 kbit/s | 9,531 | 2,414 kbit/s | 22, 30, 38 |
| 1 Mbit/s constrained link | 12.6 | 953 kbit/s | 9,472 | 947 kbit/s | 22, 30, 38 |

The host did adapt: it reduced frame rate enough to hold the stream to the
constrained link. It did not change the H.264 quantizers in this run. Microsoft
documents image quality, frame rate, and compression as possible adaptive
responses, so unchanged quantizers do not mean that adaptation failed. The
measurement does not yet explain why Microsoft's client shows a softer whole
desktop on a slow link.

## Confirmed defects

### AQ-1 — connect-time bandwidth byte count omits each header

- [x] Fixed and verified: `Detector::payload` adds the declared payload plus eight
  bytes for each Payload and connect-time Stop, and
  `a_connect_time_measurement_counts_each_payload_with_its_header` feeds 1,024
  and 512 real payload bytes and reads `byteCount` 1,552 from the encoded
  response. The laptop ran no connect-time measurement in either repeat run, so
  only the unit test exercises this path.

`src/rdp_client/proto/autodetect.rs` adds only `payloadLength` for a connect-time
Bandwidth Measure Payload and Bandwidth Measure Stop. [MS-RDPBCGR section
3.2.5.14](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf)
requires `payloadLength + 8` for each message. The reported `byteCount` is low by
eight bytes per payload and another eight bytes for the Stop.

Done when:

- both connect-time message kinds add the declared payload plus eight bytes;
- tests contain the declared payload bytes, rather than only their length
  fields; and
- a wire-level test asserts the exact `byteCount` in the response.

### AQ-2 — malformed auto-detect requests are accepted and counted

- [x] Fixed and verified: `headerLength` is checked per request type, and a
  Payload or connect-time Stop consumes its declared payload before counting it.
  `a_header_length_that_does_not_fit_the_request_is_refused` accepts each of
  the nine request shapes and refuses every other header length for each;
  `a_payload_shorter_than_it_says_is_refused` covers a truncated Payload and a
  Stop with none of its payload, neither counted.

`Detector::answer` reads but ignores `headerLength`. It also trusts
`payloadLength` without consuming that many bytes. A truncated PDU can therefore
claim bytes that never arrived; the current connect-time unit test does this
with declared payloads of 1,024 and 512 bytes.

Done when:

- `headerLength` is checked against the request type: 6 for RTT and Start, 8 for
  Payload and connect-time Stop, 6 for continuous Stop, 14 for the two-field
  Network Characteristics Results, and 18 for its three-field form;
- Payload and connect-time Stop consume and verify exactly the declared payload
  before adding it to the count; and
- malformed-length, truncated-payload, and valid-request tests cover every
  request shape the parser accepts.

### AQ-3 — continuous bandwidth measurement counts the wrong wire boundary

- [x] Fixed and verified: `autodetect::counted` counts a fast-path PDU's body
  past its header and a Send Data Indication's MCS payload, and nothing on the
  message channel, whose PDUs `Detector::answer` counts past their security
  header, the Start excepted and the Stop included. Under TLS no other PDU wears
  a security header once the connection is up.
  `a_continuous_measurement_counts_inside_the_framing` builds the frames byte by
  byte, independently of the parsers, and asserts 566 bytes across a Start, a
  long and a short fast-path PDU, an I/O-channel PDU, a virtual-channel PDU and
  the Stop.

`src/rdp_client/session.rs` calls `detector.received(frame.len())` before the MCS
message-channel packet is decoded. This counts TPKT, X.224, and MCS framing. It
also counts bytes before the RDP Security Header in the Stop packet.
[MS-RDPBCGR section
3.2.5.14](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf)
requires only bytes following an RDP Security Header, when one is present.

Done when:

- counting starts and stops at the protocol boundary specified by MS-RDPBCGR,
  including the Stop data after its Security Header but excluding outer framing;
- the count is derived from decoded boundaries rather than the complete socket
  frame length; and
- an independent test packet parser verifies the exact byte count across Start,
  ordinary data, and Stop packets.

### AQ-4 — sub-millisecond measurements report an unusable zero `timeDelta`

- [x] Fixed and verified: `timeDelta` is at least one millisecond, still
  saturating at `u32::MAX`, and `a_measurement_under_a_millisecond_says_it_took_one`
  reads 1 from the value and the encoded response. The repeat LAN run reported
  none of its 594 measurements at zero.

Elapsed time is truncated to whole milliseconds without a lower bound. The LAN
run reported nonzero bytes in zero milliseconds for 49 of 417 measurements.
[MS-RDPBCGR section
3.3.5.14](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf)
tells the server to calculate `(byteCount * 8) / timeDelta`, so zero cannot be a
usable completed result. The constrained run had no zero-duration windows, so
this defect did not cause its unchanged quantizers.

Done when:

- every completed result reports at least one millisecond, while retaining the
  existing `u32` saturation; and
- a sub-millisecond unit test asserts `timeDelta == 1` in the returned value and
  encoded response.

### AQ-5 — the link probe treats an unavailable GFX timestamp as midnight

- [x] Fixed and verified: `time_of_day` gives no sample for a zero timestamp,
  and `lag` takes the offset around the host's day. Three tests in
  `tests/rdp_link_probe.rs` cover zero, an ordinary timestamp and a frame either
  side of midnight. The repeat runs' largest "most behind" values were 44 ms on
  the LAN and 1,402 ms behind the 1 Mbit/s relay.

`tests/rdp_link_probe.rs` converts `RDPGFX_START_FRAME.timestamp == 0` to
midnight. [MS-RDPEGFX section
2.2.2.11](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPEGFX.pdf)
defines zero as unavailable. This produced the bogus 16- to
17-million-millisecond "most behind desktop" values in both logs. It does not
affect the bandwidth, frame-rate, or quantizer totals.

Done when:

- a zero timestamp contributes no lag sample;
- valid time-of-day timestamps still contribute samples, including across a UTC
  day boundary; and
- focused tests cover zero, an ordinary timestamp, and day rollover.

## Audit corrections already completed

- [x] Corrected the claim that unchanged H.264 quantizers mean no adaptation.
  The constrained host adapted by frame rate, an outcome Microsoft documents.
- [x] Corrected the claim that a missing Network Characteristics Result proves
  failure. MS-RDPBCGR section 1.3.9 does not put that message in the continuous
  exchange over the main TCP connection.
- [x] Verified that `AVC_THINCLIENT` makes this host choose whole-desktop
  AVC444v2. Advertising the flag is not the missing step in this run.
- [x] Kept production auto-detection disabled while its implementation remains
  experimental; `src/rdp.rs` intentionally passes `autodetect: false`.

## Ruled out as current bugs

- `RDPGFX_FRAME_ACKNOWLEDGE_PDU.queueDepth == 0` means that buffer information
  is unavailable. A nonzero value lets the server throttle graphics frame rate;
  MS-RDPEGFX does not define it as an image-quality instruction.
- A missing Network Characteristics Result on the main TCP continuous exchange
  is allowed. `RDP_NETCHAR_SYNC` belongs to synchronization such as reconnect,
  not to every continuous measurement.
- UDP is a remaining hypothesis, not a conclusion. The existing TCP result does
  not justify implementing MS-RDPEMT and MS-RDPEUDP yet.

## Gateway-to-host validation gate

- [x] Fix AQ-1 through AQ-5 and run the required Rust checks.
- [x] Repeat the LAN and 1 Mbit/s real-host probes. Confirm exact connect-time and
  continuous byte counts, no zero `timeDelta`, and no bogus timestamp outliers.
  `tmp/autodetect/lan-fixed.log` and `throttle1000-fixed.log`: 594 and 1,849
  measurements, none at zero; through the relay the host was told 985 kbit/s in
  all. The host ran no connect-time measurement, so its count rests on the unit
  test. Nothing else moved: 31.8 frames/s at 2,455 kbit/s on the LAN, 12.5 at
  951 kbit/s constrained, 9.5 to 9.6 KB a frame and quantizers 22, 30 and 38 in
  both, still with no Network Characteristics Result.
- [ ] Force Microsoft's client to TCP over the same constrained host link and
  collect its RemoteFX Graphics and network performance counters plus the
  `RdpCoreTS` operational log.
- [ ] If Microsoft's client softens the desktop over TCP, capture and compare its
  capability advertisement with this client before changing transport.
- [ ] If Microsoft's client softens only with its normal UDP transport, add the
  MS-RDPEMT and MS-RDPEUDP specifications to `../ms-rdp-specs` and then scope the
  multitransport implementation.
- [ ] Only after that comparison identifies a valid gateway implementation,
  decide what browser integration or browser QA is required.

## Primary references

- [MS-RDPBCGR](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf),
  especially sections 1.3.9, 2.2.14, 3.2.5.14, and 3.3.5.14
- [MS-RDPEGFX](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPEGFX.pdf),
  especially sections 2.2.2.11, 2.2.2.13, and 3.2.5.13
- [Microsoft: RDP bandwidth](https://learn.microsoft.com/en-us/azure/virtual-desktop/rdp-bandwidth)
