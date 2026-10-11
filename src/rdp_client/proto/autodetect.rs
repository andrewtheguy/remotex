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

use super::share;
use super::wire::{Malformed, Reader, Writer};

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

/// The client's side of the detection: one measurement at a time, as the host runs
/// them.
#[derive(Debug, Default)]
pub struct Detector {
    /// When the measurement under way started, and the bytes that have arrived since.
    measuring: Option<(Instant, u64)>,
}

impl Detector {
    /// Count bytes that arrived from the host, toward a measurement under way.
    pub fn received(&mut self, bytes: usize) {
        if let Some((_, counted)) = &mut self.measuring {
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
        r.u8()?; // headerLength, which the request type implies
        let kind = r.u8()?;
        if kind != TYPE_REQUEST {
            return Err(r.refuse("its header type", kind));
        }
        let sequence = r.u16_le()?;
        let request = r.u16_le()?;
        Ok(match request {
            RTT_CONTINUOUS | RTT_CONNECT => Answer::Reply(response(sequence, RTT_RESPONSE, None)),
            BW_START_CONTINUOUS | BW_START_CONNECT => {
                self.measuring = Some((now, 0));
                Answer::Nothing
            }
            // A connect-time measurement counts its payloads alone, as 2.2.14.2.2
            // has it; the frames they arrive in are counted by [`Self::received`]
            // only once the connection is up.
            BW_PAYLOAD => {
                let length = r.u16_le()?;
                self.received(usize::from(length));
                Answer::Nothing
            }
            BW_STOP_CONTINUOUS | BW_STOP_CONNECT => {
                let results = if request == BW_STOP_CONNECT {
                    let length = r.u16_le()?;
                    self.received(usize::from(length));
                    BW_RESULTS_CONNECT
                } else {
                    BW_RESULTS_CONTINUOUS
                };
                let Some((started, bytes)) = self.measuring.take() else {
                    return Ok(Answer::Nothing);
                };
                let millis = u32::try_from(now.saturating_duration_since(started).as_millis()).unwrap_or(u32::MAX);
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

    fn request(kind: u16, sequence: u16, extra: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x00, 0x10, 0x00, 0x00, 6 + extra.len() as u8, TYPE_REQUEST];
        bytes.extend_from_slice(&sequence.to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(extra);
        bytes
    }

    #[test]
    fn a_round_trip_is_answered_at_once() {
        let mut detector = Detector::default();
        let answer = detector.answer(&request(RTT_CONTINUOUS, 7, &[]), Instant::now()).unwrap();
        assert_eq!(answer, Answer::Reply(vec![0x00, 0x20, 0x00, 0x00, 0x06, 0x01, 7, 0, 0, 0]));
    }

    /// Continuous: what arrived between the start and the stop, and how long it took.
    #[test]
    fn a_measurement_reports_the_bytes_between_its_start_and_its_stop() {
        let mut detector = Detector::default();
        let start = Instant::now();
        detector.received(999); // before the start: not counted
        assert_eq!(detector.answer(&request(BW_START_CONTINUOUS, 1, &[]), start).unwrap(), Answer::Nothing);
        detector.received(1000);
        detector.received(500);
        let answer = detector.answer(&request(BW_STOP_CONTINUOUS, 2, &[]), start + Duration::from_millis(250)).unwrap();
        let Answer::Measured { reply, millis, bytes } = answer else { panic!("{answer:?}") };
        assert_eq!((millis, bytes), (250, 1500));
        assert_eq!(reply[..10], [0x00, 0x20, 0x00, 0x00, 0x0E, 0x01, 2, 0, 0x0B, 0x00]);
        assert_eq!(reply[10..], [250, 0, 0, 0, 0xDC, 0x05, 0, 0]);
        detector.received(1000); // after the stop: not counted
        assert_eq!(detector.answer(&request(BW_STOP_CONTINUOUS, 3, &[]), start).unwrap(), Answer::Nothing);
    }

    #[test]
    fn a_connect_time_measurement_counts_its_payloads() {
        let mut detector = Detector::default();
        let start = Instant::now();
        detector.answer(&request(BW_START_CONNECT, 1, &[]), start).unwrap();
        detector.answer(&request(BW_PAYLOAD, 2, &[0x00, 0x04]), start).unwrap();
        let answer =
            detector.answer(&request(BW_STOP_CONNECT, 3, &[0x00, 0x02]), start + Duration::from_millis(10)).unwrap();
        let Answer::Measured { reply, millis, bytes } = answer else { panic!("{answer:?}") };
        assert_eq!((millis, bytes), (10, 1536));
        assert_eq!(reply[8..10], BW_RESULTS_CONNECT.to_le_bytes());
    }

    #[test]
    fn the_hosts_conclusion_is_read_by_its_type() {
        let mut detector = Detector::default();
        let fields = [20u32, 2000, 25].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>();
        let answer = detector.answer(&request(0x08C0, 1, &fields), Instant::now()).unwrap();
        assert_eq!(
            answer,
            Answer::Concluded(Characteristics {
                base_rtt_ms: Some(20),
                bandwidth_kbps: Some(2000),
                average_rtt_ms: Some(25),
            })
        );
    }

    #[test]
    fn a_pdu_without_the_request_flag_is_refused() {
        let mut bytes = request(RTT_CONTINUOUS, 1, &[]);
        bytes[1] = 0;
        assert!(Detector::default().answer(&bytes, Instant::now()).is_err());
    }
}
