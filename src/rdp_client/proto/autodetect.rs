//! Network characteristics detection ([MS-RDPBCGR] 2.2.14): the host measures the
//! link to this client, and the client answers its measurements.
//!
//! Only a client that claims it in the core data takes part, and the PDUs travel on
//! the MCS message channel alone, which the client asks for there too
//! ([`super::gcc::ConferenceCreateRequest::autodetect`]). The host times a round
//! trip with a request this client answers at once, and the bandwidth by bracketing
//! data with a start and a stop: during the connection with payloads of its own,
//! and afterwards around whatever it was sending anyway. The client says how long it
//! took between the two and how many bytes arrived meanwhile, and the host may tell
//! it what it concluded.
//!
//! What the measurements are taken of is the connection this client reads. It is
//! not the link a page behind a gateway is on, and nothing here pretends otherwise.

use std::time::Instant;

use super::wire::{Malformed, Reader, Writer};
use super::{fastpath, mcs, share};

/// `SEC_AUTODETECT_REQ`, on every request the host sends.
const REQUEST: u16 = 0x1000;
/// `SEC_AUTODETECT_RSP`, on every response.
const RESPONSE: u16 = 0x2000;

/// `TYPE_ID_AUTODETECT_REQUEST` and `TYPE_ID_AUTODETECT_RESPONSE`.
const TYPE_REQUEST: u8 = 0x00;
const TYPE_RESPONSE: u8 = 0x01;

/// What a request may be, by its `requestType`.
const RTT_CONTINUOUS: u16 = 0x0001;
const RTT_CONNECT: u16 = 0x1001;
const BW_START_CONTINUOUS: u16 = 0x0014;
const BW_START_CONNECT: u16 = 0x1014;
const BW_PAYLOAD: u16 = 0x0002;
const BW_STOP_CONTINUOUS: u16 = 0x0429;
const BW_STOP_CONNECT: u16 = 0x002B;
const NETCHAR_RESULTS: [u16; 3] = [0x0840, 0x0880, 0x08C0];

/// What a response is, by its `responseType`.
const RTT_RESPONSE: u16 = 0x0000;
const BW_RESULTS_CONNECT: u16 = 0x0003;
const BW_RESULTS_CONTINUOUS: u16 = 0x000B;

/// What the host concluded of the link, as it said in a Network Characteristics
/// Result. Each field is absent when the host left it out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Characteristics {
    pub base_rtt_ms: Option<u32>,
    pub bandwidth_kbps: Option<u32>,
    pub average_rtt_ms: Option<u32>,
}

/// What one request comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Nothing to send.
    Nothing,
    /// A response to send on the message channel, security header and all.
    Reply(Vec<u8>),
    /// The results of a bandwidth measurement, sent as `reply`.
    Measured { reply: Vec<u8>, millis: u32, bytes: u32 },
    /// The host's conclusion.
    Concluded(Characteristics),
}

/// How a measurement under way counts what arrives ([MS-RDPBCGR] 3.2.5.14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Counting {
    /// Connect-time: each Payload, and the Stop, with its eight bytes of header.
    Payloads,
    /// Continuous: every byte from the host, past any security header, until the
    /// Stop, the Stop's own included.
    Everything,
}

/// The client's side of the detection: one measurement at a time, as the host runs
/// them.
#[derive(Debug, Default)]
pub struct Detector {
    /// When the measurement under way started, how it counts, and what it has
    /// counted since.
    measuring: Option<(Instant, Counting, u64)>,
}

impl Detector {
    /// Count bytes that arrived from the host off the message channel, past any
    /// security header, toward a continuous measurement under way. What arrives on
    /// the message channel is counted by [`Self::answer`].
    pub fn received(&mut self, bytes: usize) {
        if let Some((_, Counting::Everything, counted)) = &mut self.measuring {
            *counted += bytes as u64;
        }
    }

    /// One Auto-Detect Request PDU, from its security header on.
    pub fn answer(&mut self, payload: &[u8], now: Instant) -> Result<Answer, Malformed> {
        const WHAT: &str = "an Auto-Detect Request PDU";
        let mut r = Reader::new(WHAT, payload);
        let flags = share::read_security_header(&mut r)?;
        if flags & REQUEST == 0 {
            return Err(r.refuse("its security flags", flags));
        }
        // What a continuous measurement counts of this PDU: everything past its
        // security header, unless it is the Start, before which nothing counts.
        let after_header = r.rest().len();
        let header_length = r.u8()?;
        let kind = r.u8()?;
        if kind != TYPE_REQUEST {
            return Err(r.refuse("its header type", kind));
        }
        let sequence = r.u16_le()?;
        let request = r.u16_le()?;
        let expected: u8 = match request {
            BW_PAYLOAD | BW_STOP_CONNECT => 0x08,
            0x0840 | 0x0880 => 0x0E,
            0x08C0 => 0x12,
            _ => 0x06,
        };
        if header_length != expected {
            return Err(r.refuse("its header length", header_length));
        }
        if !matches!(request, BW_START_CONTINUOUS | BW_START_CONNECT) {
            self.received(after_header);
        }
        Ok(match request {
            RTT_CONTINUOUS | RTT_CONNECT => Answer::Reply(response(sequence, RTT_RESPONSE, None)),
            BW_START_CONTINUOUS => {
                self.measuring = Some((now, Counting::Everything, 0));
                Answer::Nothing
            }
            BW_START_CONNECT => {
                self.measuring = Some((now, Counting::Payloads, 0));
                Answer::Nothing
            }
            BW_PAYLOAD => {
                self.payload(&mut r)?;
                Answer::Nothing
            }
            BW_STOP_CONTINUOUS | BW_STOP_CONNECT => {
                let results = if request == BW_STOP_CONNECT {
                    self.payload(&mut r)?;
                    BW_RESULTS_CONNECT
                } else {
                    BW_RESULTS_CONTINUOUS
                };
                let Some((started, _, bytes)) = self.measuring.take() else {
                    return Ok(Answer::Nothing);
                };
                // The host divides by it (3.3.5.14), so a measurement shorter than a
                // millisecond is said to have taken one.
                let millis =
                    u32::try_from(now.saturating_duration_since(started).as_millis()).unwrap_or(u32::MAX).max(1);
                let bytes = u32::try_from(bytes).unwrap_or(u32::MAX);
                Answer::Measured { reply: response(sequence, results, Some((millis, bytes))), millis, bytes }
            }
            request if NETCHAR_RESULTS.contains(&request) => {
                let mut said = Characteristics::default();
                if request & 0x0040 != 0 {
                    said.base_rtt_ms = Some(r.u32_le()?);
                }
                if request & 0x0080 != 0 {
                    said.bandwidth_kbps = Some(r.u32_le()?);
                }
                said.average_rtt_ms = Some(r.u32_le()?);
                Answer::Concluded(said)
            }
            request => return Err(r.refuse("its request type", request)),
        })
    }

    /// A connect-time Payload's or Stop's `payloadLength` and its payload, which must
    /// all have arrived, counted with the eight bytes of header before it.
    fn payload(&mut self, r: &mut Reader<'_>) -> Result<(), Malformed> {
        let length = usize::from(r.u16_le()?);
        r.bytes(length)?;
        if let Some((_, Counting::Payloads, counted)) = &mut self.measuring {
            *counted += length as u64 + 8;
        }
        Ok(())
    }
}

/// What a continuous measurement counts of one frame from the host
/// ([MS-RDPBCGR] 3.2.5.14): the data the framing carries, and none of the framing.
///
/// A fast-path PDU counts its updates past its own header, and a Send Data
/// Indication its MCS payload past TPKT, X.224 and MCS. Under TLS no PDU off the
/// message channel wears an RDP Security Header once the connection is up, so
/// there is none to step over. One on the message channel counts for nothing here:
/// [`Detector::answer`] counts it past its security header, and the Start not at
/// all. A server's disconnect carries no data.
pub fn counted(frame: &[u8], message_channel: Option<u16>) -> Result<usize, Malformed> {
    if frame.first().is_some_and(|&first| fastpath::is_output(first)) {
        return Ok(fastpath::body(frame)?.len());
    }
    Ok(match mcs::send_data_indication(frame)? {
        mcs::Indication::Data(data) if Some(data.channel) != message_channel => data.payload.len(),
        mcs::Indication::Data(_) | mcs::Indication::Disconnect(_) => 0,
    })
}

/// An Auto-Detect Response PDU: the security header, and an RTT Measure Response or,
/// with `results`, Bandwidth Measure Results.
fn response(sequence: u16, kind: u16, results: Option<(u32, u32)>) -> Vec<u8> {
    let mut w = Writer::with_capacity(4 + 14);
    share::write_security_header(&mut w, RESPONSE);
    w.u8(if results.is_some() { 0x0E } else { 0x06 });
    w.u8(TYPE_RESPONSE);
    w.u16_le(sequence);
    w.u16_le(kind);
    if let Some((millis, bytes)) = results {
        w.u32_le(millis);
        w.u32_le(bytes);
    }
    w.finish()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// An Auto-Detect Request PDU from its security header on, written field by field
    /// as 2.2.14.1 lays each request out: `header_length` as given, so a test can lie.
    fn request(header_length: u8, kind: u16, sequence: u16, rest: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x00, 0x10, 0x00, 0x00, header_length, TYPE_REQUEST];
        bytes.extend_from_slice(&sequence.to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(rest);
        bytes
    }

    /// A Payload or connect-time Stop: its `payloadLength` and that many bytes.
    fn with_payload(kind: u16, sequence: u16, payload: &[u8]) -> Vec<u8> {
        let mut rest = u16::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
        rest.extend_from_slice(payload);
        request(0x08, kind, sequence, &rest)
    }

    /// The `timeDelta` and `byteCount` a Bandwidth Measure Results says, read from the
    /// bytes rather than taken from [`Answer::Measured`].
    fn results(reply: &[u8]) -> (u16, u32, u32) {
        assert_eq!(reply[..6], [0x00, 0x20, 0x00, 0x00, 0x0E, TYPE_RESPONSE]);
        let field = |at: usize| u32::from_le_bytes(reply[at..at + 4].try_into().unwrap());
        (u16::from_le_bytes([reply[8], reply[9]]), field(10), field(14))
    }

    #[test]
    fn a_round_trip_is_answered_at_once() {
        let mut detector = Detector::default();
        for kind in [RTT_CONTINUOUS, RTT_CONNECT] {
            let answer = detector.answer(&request(0x06, kind, 7, &[]), Instant::now()).unwrap();
            assert_eq!(answer, Answer::Reply(vec![0x00, 0x20, 0x00, 0x00, 0x06, 0x01, 7, 0, 0, 0]));
        }
    }

    /// Connect-time: each Payload and the Stop count their payload and their eight
    /// bytes of header, and nothing else that arrives does.
    #[test]
    fn a_connect_time_measurement_counts_each_payload_with_its_header() {
        let mut detector = Detector::default();
        let start = Instant::now();
        detector.answer(&request(0x06, BW_START_CONNECT, 1, &[]), start).unwrap();
        detector.received(5000);
        detector.answer(&with_payload(BW_PAYLOAD, 2, &[0xAB; 1024]), start).unwrap();
        detector.answer(&request(0x06, RTT_CONNECT, 3, &[]), start).unwrap();
        let stop = with_payload(BW_STOP_CONNECT, 4, &[0xCD; 512]);
        let answer = detector.answer(&stop, start + Duration::from_millis(10)).unwrap();
        let Answer::Measured { reply, millis, bytes } = answer else { panic!("{answer:?}") };
        assert_eq!((millis, bytes), (10, 1024 + 8 + 512 + 8));
        assert_eq!(results(&reply), (BW_RESULTS_CONNECT, 10, 1552));
        assert_eq!(reply[6..8], 4u16.to_le_bytes(), "the Stop's sequence number");
    }

    /// Continuous: what arrived off the message channel, and on it everything past
    /// each security header but the Start's — the Stop's own six bytes included.
    #[test]
    fn a_continuous_measurement_counts_what_arrived_between_its_start_and_its_stop() {
        let mut detector = Detector::default();
        let start = Instant::now();
        detector.received(999); // before the start: not counted
        assert_eq!(detector.answer(&request(0x06, BW_START_CONTINUOUS, 1, &[]), start).unwrap(), Answer::Nothing);
        detector.received(1000);
        detector.answer(&request(0x06, RTT_CONTINUOUS, 2, &[]), start).unwrap();
        detector.received(500);
        let answer = detector.answer(&request(0x06, BW_STOP_CONTINUOUS, 3, &[]), start + Duration::from_millis(250));
        let Answer::Measured { reply, millis, bytes } = answer.unwrap() else { panic!() };
        assert_eq!((millis, bytes), (250, 1000 + 6 + 500 + 6));
        assert_eq!(results(&reply), (BW_RESULTS_CONTINUOUS, 250, 1512));
        detector.received(1000); // after the stop: not counted
        let late = detector.answer(&request(0x06, BW_STOP_CONTINUOUS, 4, &[]), start).unwrap();
        assert_eq!(late, Answer::Nothing, "a Stop with no Start under way");
    }

    /// A Start under way is started over by the next.
    #[test]
    fn a_second_start_starts_the_measurement_over() {
        let mut detector = Detector::default();
        let start = Instant::now();
        detector.answer(&request(0x06, BW_START_CONTINUOUS, 1, &[]), start).unwrap();
        detector.received(1000);
        let again = start + Duration::from_millis(5);
        detector.answer(&request(0x06, BW_START_CONTINUOUS, 2, &[]), again).unwrap();
        detector.received(10);
        let answer = detector.answer(&request(0x06, BW_STOP_CONTINUOUS, 3, &[]), again + Duration::from_millis(2));
        let Answer::Measured { millis, bytes, .. } = answer.unwrap() else { panic!() };
        assert_eq!((millis, bytes), (2, 16));
    }

    /// The host divides by `timeDelta`, so a measurement under a millisecond says one.
    #[test]
    fn a_measurement_under_a_millisecond_says_it_took_one() {
        let mut detector = Detector::default();
        let start = Instant::now();
        detector.answer(&request(0x06, BW_START_CONTINUOUS, 1, &[]), start).unwrap();
        detector.received(3000);
        let answer = detector.answer(&request(0x06, BW_STOP_CONTINUOUS, 2, &[]), start + Duration::from_micros(400));
        let Answer::Measured { reply, millis, bytes } = answer.unwrap() else { panic!() };
        assert_eq!((millis, bytes), (1, 3006));
        assert_eq!(results(&reply), (BW_RESULTS_CONTINUOUS, 1, 3006));
    }

    #[test]
    fn the_hosts_conclusion_is_read_in_each_of_its_shapes() {
        let fields = |values: &[u32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>();
        let cases = [
            (0x0E, 0x0840, vec![20, 25], Characteristics { base_rtt_ms: Some(20), bandwidth_kbps: None, average_rtt_ms: Some(25) }),
            (0x0E, 0x0880, vec![2000, 25], Characteristics { base_rtt_ms: None, bandwidth_kbps: Some(2000), average_rtt_ms: Some(25) }),
            (
                0x12,
                0x08C0,
                vec![20, 2000, 25],
                Characteristics { base_rtt_ms: Some(20), bandwidth_kbps: Some(2000), average_rtt_ms: Some(25) },
            ),
        ];
        for (length, kind, values, said) in cases {
            let answer = Detector::default().answer(&request(length, kind, 1, &fields(&values)), Instant::now());
            assert_eq!(answer.unwrap(), Answer::Concluded(said), "{kind:#06x}");
        }
    }

    /// Every request with the header length of another shape is refused, and
    /// counted for nothing.
    #[test]
    fn a_header_length_that_does_not_fit_the_request_is_refused() {
        let shapes: [(u16, u8, &[u8]); 9] = [
            (RTT_CONTINUOUS, 0x06, &[]),
            (RTT_CONNECT, 0x06, &[]),
            (BW_START_CONTINUOUS, 0x06, &[]),
            (BW_START_CONNECT, 0x06, &[]),
            (BW_PAYLOAD, 0x08, &[0x02, 0x00, 0xAA, 0xBB]),
            (BW_STOP_CONNECT, 0x08, &[0x02, 0x00, 0xAA, 0xBB]),
            (BW_STOP_CONTINUOUS, 0x06, &[]),
            (0x0880, 0x0E, &[0; 8]),
            (0x08C0, 0x12, &[0; 12]),
        ];
        for (kind, right, rest) in shapes {
            let mut detector = Detector::default();
            assert!(detector.answer(&request(right, kind, 1, rest), Instant::now()).is_ok(), "{kind:#06x}");
            for wrong in [0x00, 0x06, 0x08, 0x0E, 0x12].into_iter().filter(|&wrong| wrong != right) {
                let refused = Detector::default().answer(&request(wrong, kind, 1, rest), Instant::now());
                assert!(refused.is_err(), "{kind:#06x} with a header length of {wrong}");
            }
        }
    }

    /// A Payload or connect-time Stop that declares more than it carries is refused,
    /// and the bytes it claimed are not counted.
    #[test]
    fn a_payload_shorter_than_it_says_is_refused() {
        let mut detector = Detector::default();
        let start = Instant::now();
        detector.answer(&request(0x06, BW_START_CONNECT, 1, &[]), start).unwrap();
        let mut short = with_payload(BW_PAYLOAD, 2, &[0xAB; 1024]);
        short.truncate(short.len() - 1);
        assert!(detector.answer(&short, start).is_err());
        let stop = with_payload(BW_STOP_CONNECT, 3, &[0xCD; 16]);
        assert!(detector.answer(&stop[..stop.len() - 16], start).is_err(), "a Stop with none of its payload");
        let Answer::Measured { bytes, .. } = detector.answer(&stop, start).unwrap() else { panic!() };
        assert_eq!(bytes, 16 + 8, "only the Stop that arrived whole");
    }

    #[test]
    fn a_pdu_without_the_request_flag_is_refused() {
        let mut bytes = request(0x06, RTT_CONTINUOUS, 1, &[]);
        bytes[1] = 0;
        assert!(Detector::default().answer(&bytes, Instant::now()).is_err());
    }

    #[test]
    fn an_unknown_request_type_is_refused() {
        assert!(Detector::default().answer(&request(0x06, 0x0003, 1, &[]), Instant::now()).is_err());
    }
    /// A Send Data Indication built byte by byte from T.123, X.224 and T.125,
    /// independently of the parser: TPKT, an X.224 data header, then the MCS
    /// CHOICE, initiator, channel, flags, a PER length and the payload.
    fn indication(channel: u16, payload: &[u8]) -> Vec<u8> {
        let mut mcs = vec![26 << 2, 0x00, 0x06]; // SDin, initiator 1007
        mcs.extend_from_slice(&channel.to_be_bytes());
        mcs.push(0x70);
        match payload.len() {
            n @ 0..0x80 => mcs.push(n as u8),
            n => mcs.extend_from_slice(&(0x8000 | n as u16).to_be_bytes()),
        }
        mcs.extend_from_slice(payload);
        let total = 4 + 3 + mcs.len();
        let mut frame = vec![0x03, 0x00];
        frame.extend_from_slice(&(total as u16).to_be_bytes());
        frame.extend_from_slice(&[0x02, 0xF0, 0x80]);
        frame.extend_from_slice(&mcs);
        frame
    }

    /// A fast-path output PDU of one update, its length in one byte or two.
    fn fast_path(update: &[u8]) -> Vec<u8> {
        let short = 2 + update.len();
        let mut frame = if short < 0x80 {
            vec![0x00, short as u8]
        } else {
            let long = (3 + update.len()) as u16;
            vec![0x00, 0x80 | (long >> 8) as u8, long as u8]
        };
        frame.extend_from_slice(update);
        frame
    }

    /// The bytes a continuous measurement counts are the data inside each frame:
    /// a Start on the message channel, then a fast-path update, a slow-path PDU on
    /// the I/O channel, one on a virtual channel, and the Stop.
    #[test]
    fn a_continuous_measurement_counts_inside_the_framing() {
        const MESSAGE: u16 = 1008;
        let mut detector = Detector::default();
        let start = Instant::now();
        // As the session reads: each frame counted inside its framing, and one on the
        // message channel answered.
        let feed = |detector: &mut Detector, frame: &[u8], now: Instant| {
            detector.received(counted(frame, Some(MESSAGE)).unwrap());
            let Ok(mcs::Indication::Data(data)) = mcs::send_data_indication(frame) else { return None };
            (data.channel == MESSAGE).then(|| detector.answer(data.payload, now).unwrap())
        };
        let begun = feed(&mut detector, &indication(MESSAGE, &request(0x06, BW_START_CONTINUOUS, 1, &[])), start);
        assert_eq!(begun, Some(Answer::Nothing));
        let fast = fast_path(&[0x01; 300]);
        assert_eq!(fast.len(), 303, "a long length");
        feed(&mut detector, &fast, start);
        feed(&mut detector, &fast_path(&[0x02; 40]), start);
        feed(&mut detector, &indication(1003, &[0x03; 200]), start);
        feed(&mut detector, &indication(1004, &[0x04; 20]), start);
        let ended = start + Duration::from_millis(40);
        let stop = feed(&mut detector, &indication(MESSAGE, &request(0x06, BW_STOP_CONTINUOUS, 2, &[])), ended).unwrap();
        let Answer::Measured { millis, bytes, .. } = stop else { panic!("{stop:?}") };
        assert_eq!((millis, bytes), (40, 300 + 40 + 200 + 20 + 6));
    }
}
