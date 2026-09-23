//! One sender's RTSP conversation: OPTIONS with its Apple-Challenge, ANNOUNCE
//! with the codec and the wrapped AES key, SETUP for the UDP ports, RECORD, then
//! SET_PARAMETER and FLUSH until TEARDOWN — every request after the Digest
//! challenge the password answers. A stream is set up only while an Apple audio
//! session is running, and the connection is closed when that session ends.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use base64::Engine as _;
use log::{debug, info, warn};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::watch;

use super::Shared;
use super::crypto::{B64, REALM, apple_response, digest_matches, unwrap_aes_key};
use super::rtp::{Codec, Params, Stream};

/// The most a request's headers may take, and its body: a sender's are a few
/// hundred bytes and a few kilobytes of SDP, and this is a socket on the LAN
/// without a password yet.
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;

/// The most frames an ALAC packet may be announced with: ALAC's own default, and
/// far more than fits in one UDP packet of 16-bit stereo.
const MAX_ALAC_FRAMES: u32 = 4096;

struct Request {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16) -> Self {
        Self { status, headers: Vec::new(), body: Vec::new() }
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

pub(super) async fn serve(tcp: TcpStream, id: u64, shared: Arc<Shared>) {
    let peer = tcp.peer_addr().map(|a| a.ip().to_canonical());
    info!("airplay #{id}: a sender connected from {}", peer.map_or("?".into(), |ip| ip.to_string()));
    let mut conn = Conn {
        id,
        shared,
        nonce: uuid::Uuid::new_v4().simple().to_string(),
        authorized: false,
        params: None,
        stream: None,
        session: None,
    };
    if let Err(e) = conn.run(tcp).await {
        warn!("airplay #{id}: {e:#}");
    }
    conn.stop();
    info!("airplay #{id}: the sender left");
}

struct Conn {
    id: u64,
    shared: Arc<Shared>,
    /// This connection's Digest challenge.
    nonce: String,
    /// The password has been answered; RTSP carries it on every request until then.
    authorized: bool,
    params: Option<Params>,
    stream: Option<Stream>,
    /// The Apple audio session this connection's stream was set up under.
    session: Option<u64>,
}

impl Conn {
    async fn run(&mut self, tcp: TcpStream) -> anyhow::Result<()> {
        // Whole, IPv6 scope included: a link-local address is bound and reached
        // through its interface.
        let local = tcp.local_addr()?;
        let peer = tcp.peer_addr()?;
        let (reader, mut writer) = tcp.into_split();
        let mut reader = BufReader::new(reader);
        let mut sessions = self.shared.session.subscribe();
        loop {
            // Cancelling a half-read request is harmless: the connection ends here.
            let request = tokio::select! {
                request = read_request(&mut reader) => request?,
                () = session_ended(&mut sessions, self.session) => {
                    info!("airplay #{}: the session ended; hanging up on the sender", self.id);
                    break;
                }
            };
            let Some(request) = request else { break };
            debug!("airplay #{}: {} {}", self.id, request.method, request.uri);
            let cseq = request.header("CSeq").unwrap_or("0").to_owned();
            let closing = request.method == "TEARDOWN";
            let mut response = if self.authorize(&request) {
                self.handle(&request, local, peer).await.unwrap_or_else(|e| {
                    warn!("airplay #{}: {} failed: {e:#}", self.id, request.method);
                    Response::new(400)
                })
            } else {
                Response::new(401).header(
                    "WWW-Authenticate",
                    format!(r#"Digest realm="{REALM}", nonce="{}""#, self.nonce),
                )
            };
            // Answered on whatever it arrives with, a refusal included: a sender
            // that cannot verify the speaker never gets as far as the password.
            if let Some(challenge) = request.header("Apple-Challenge") {
                match apple_response(challenge, local.ip().to_canonical(), self.shared.hw_addr) {
                    Ok(answer) => response = response.header("Apple-Response", answer),
                    Err(e) => warn!("airplay #{}: {e:#}", self.id),
                }
            }
            write_response(&mut writer, &cseq, response).await?;
            if closing {
                break;
            }
        }
        Ok(())
    }

    fn authorize(&mut self, request: &Request) -> bool {
        if self.authorized {
            return true;
        }
        let Some(authorization) = request.header("Authorization") else {
            return false;
        };
        self.authorized =
            digest_matches(authorization, &request.method, &self.nonce, &self.shared.password);
        if !self.authorized {
            warn!("airplay #{}: the sender's password is wrong", self.id);
        }
        self.authorized
    }

    async fn handle(&mut self, request: &Request, local: SocketAddr, peer: SocketAddr) -> anyhow::Result<Response> {
        Ok(match request.method.as_str() {
            "OPTIONS" => Response::new(200).header(
                "Public",
                "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER",
            ),
            "ANNOUNCE" => {
                let sdp = std::str::from_utf8(&request.body).context("the SDP is not UTF-8")?;
                self.params = Some(parse_sdp(sdp)?);
                Response::new(200)
            }
            "SETUP" => {
                let params = self.params.clone().context("SETUP before ANNOUNCE")?;
                let Some(session) = *self.shared.session.borrow() else {
                    warn!("airplay #{}: refused, no Apple audio session is running", self.id);
                    return Ok(Response::new(453));
                };
                {
                    let mut streaming = self.shared.streaming.lock().unwrap();
                    match *streaming {
                        Some(other) if other != self.id => {
                            warn!("airplay #{}: refused, #{other} is already streaming", self.id);
                            return Ok(Response::new(453));
                        }
                        _ => *streaming = Some(self.id),
                    }
                }
                let transport = request.header("Transport").unwrap_or_default();
                let timing_port = transport_port(transport, "timing_port");
                self.stream = None;
                let stream = match Stream::start(local, peer, timing_port, params, Arc::clone(&self.shared)).await {
                    Ok(stream) => stream,
                    // The claim goes with it, or the connection would hold the one
                    // stream with nothing playing it.
                    Err(e) => {
                        self.stop();
                        return Err(e);
                    }
                };
                let reply = format!(
                    "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={};server_port={}",
                    stream.control_port, stream.timing_port, stream.audio_port
                );
                info!("airplay #{}: streaming to UDP port {}", self.id, stream.audio_port);
                self.stream = Some(stream);
                self.session = Some(session);
                Response::new(200).header("Transport", reply).header("Session", "1")
            }
            "RECORD" => Response::new(200).header("Audio-Latency", "11025"),
            // Volume, progress and metadata. An AirPlay 1 sender leaves volume to
            // the speaker, and this one leaves it to the browser: the Mac's slider
            // changes nothing that is heard.
            "SET_PARAMETER" | "FLUSH" => Response::new(200),
            "GET_PARAMETER" => {
                let mut response = Response::new(200);
                if String::from_utf8_lossy(&request.body).contains("volume") {
                    response = response.header("Content-Type", "text/parameters");
                    response.body = b"volume: 0.000000\r\n".to_vec();
                }
                response
            }
            "TEARDOWN" => {
                self.stop();
                Response::new(200)
            }
            // AirPlay 2's GET /info and POST /pair-*, among others: this speaker
            // advertises itself as AirPlay 1 and answers them as shairport-sync's
            // classic mode does.
            _ => Response::new(501),
        })
    }

    fn stop(&mut self) {
        self.stream = None;
        self.session = None;
        let mut streaming = self.shared.streaming.lock().unwrap();
        if *streaming == Some(self.id) {
            *streaming = None;
        }
    }
}

/// Resolves once `session` is no longer the one running; never for a connection
/// with no stream, which the speaker has no reason to hang up on.
async fn session_ended(sessions: &mut watch::Receiver<Option<u64>>, session: Option<u64>) {
    let Some(id) = session else {
        return std::future::pending().await;
    };
    // The sender lives in `Shared`, which this connection holds.
    let _ = sessions.wait_for(|running| *running != Some(id)).await;
}

fn transport_port(transport: &str, key: &str) -> Option<u16> {
    transport
        .split(';')
        .find_map(|part| part.trim().strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

fn parse_sdp(sdp: &str) -> anyhow::Result<Params> {
    let attr = |name: &str| {
        sdp.lines()
            .find_map(|line| line.trim().strip_prefix("a=")?.strip_prefix(name))
            .map(str::trim)
    };
    let rtpmap = attr("rtpmap:").context("the SDP has no rtpmap")?;
    let encoding = rtpmap.split_whitespace().nth(1).unwrap_or_default();
    let codec = if encoding == "AppleLossless" {
        let fmtp = attr("fmtp:").context("an AppleLossless SDP has no fmtp")?;
        // After the payload type: "96 352 0 16 40 10 14 2 255 0 0 44100".
        let params = fmtp.split_once(' ').map_or("", |(_, rest)| rest);
        let info = alac::StreamInfo::from_sdp_format_parameters(params)
            .map_err(|e| anyhow::anyhow!("the fmtp {params:?} is not ALAC's: {e:?}"))?;
        if info.sample_rate() != 44_100 || info.channels() != 2 || info.bit_depth() != 16 {
            bail!("the sender offered ALAC as {params:?}, and only 44.1 kHz 16-bit stereo is taken");
        }
        // The decoder's buffers are sized by it, and a Mac sends 352.
        if info.max_frames_per_packet() > MAX_ALAC_FRAMES {
            bail!("the sender offered ALAC packets of {} frames, more than {MAX_ALAC_FRAMES}", info.max_frames_per_packet());
        }
        Codec::Alac(info)
    } else if encoding.eq_ignore_ascii_case("L16/44100/2") {
        Codec::L16
    } else {
        bail!("the sender offered {encoding:?}, which is neither ALAC nor L16");
    };
    let aes = match (attr("rsaaeskey:"), attr("aesiv:")) {
        (Some(key), Some(iv)) => {
            let key = unwrap_aes_key(key)?;
            let iv = B64.decode(iv).context("the aesiv is not base64")?;
            let len = iv.len();
            let iv: [u8; 16] = iv
                .try_into()
                .map_err(|_| anyhow::anyhow!("the aesiv is {len} bytes, not 16"))?;
            Some((key, iv))
        }
        _ => None,
    };
    Ok(Params { codec, aes })
}

async fn read_request<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> anyhow::Result<Option<Request>> {
    let mut line = String::new();
    // The request line, blank lines before it, and the headers all draw on one
    // budget, taken from as each line is read so that no line outgrows it.
    let mut budget = MAX_HEADER_BYTES;
    loop {
        line.clear();
        if read_line(reader, &mut line, &mut budget).await? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let uri = parts.next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    loop {
        line.clear();
        if read_line(reader, &mut line, &mut budget).await? == 0 {
            bail!("the connection closed inside a request's headers");
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let mut request = Request { method, uri, headers, body: Vec::new() };
    let length: usize = request
        .header("Content-Length")
        .map(str::parse)
        .transpose()
        .context("the Content-Length is not a number")?
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        bail!("a request's body is {length} bytes, more than {MAX_BODY_BYTES}");
    }
    request.body.resize(length, 0);
    reader.read_exact(&mut request.body).await.context("reading a request's body")?;
    Ok(Some(request))
}

/// Read one line into `line`, reading no more than what is left of `budget` and
/// taking from it what the line used. `0` is the connection closing.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut String,
    budget: &mut usize,
) -> anyhow::Result<usize> {
    let n = (&mut *reader).take(*budget as u64).read_line(line).await?;
    *budget -= n;
    if n > 0 && !line.ends_with('\n') && *budget == 0 {
        bail!("a request's headers ran past {MAX_HEADER_BYTES} bytes");
    }
    Ok(n)
}

async fn write_response(writer: &mut OwnedWriteHalf, cseq: &str, response: Response) -> anyhow::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        453 => "Not Enough Bandwidth",
        _ => "Not Implemented",
    };
    let mut out = format!(
        "RTSP/1.0 {} {reason}\r\nCSeq: {cseq}\r\nServer: AirTunes/105.1\r\nAudio-Jack-Status: connected; type=analog\r\n",
        response.status
    );
    for (k, v) in &response.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !response.body.is_empty() {
        out.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&response.body);
    writer.write_all(&bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_macos_style_sdp_parses() {
        let sdp = "v=0\r\no=iTunes 1 0 IN IP4 10.0.0.2\r\ns=iTunes\r\nc=IN IP4 10.0.0.3\r\nt=0 0\r\n\
                   m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
                   a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";
        let params = parse_sdp(sdp).unwrap();
        assert!(matches!(params.codec, Codec::Alac(ref i) if i.max_frames_per_packet() == 352));
        assert!(params.aes.is_none());
    }

    #[test]
    fn a_format_other_than_cd_quality_is_refused() {
        let sdp = "a=rtpmap:96 AppleLossless\r\na=fmtp:96 352 0 24 40 10 14 2 255 0 0 48000\r\n";
        let Err(err) = parse_sdp(sdp) else { panic!("a 24-bit 48 kHz stream was taken") };
        assert!(format!("{err:#}").contains("44.1 kHz"));
    }

    #[test]
    fn an_oversized_alac_packet_is_refused() {
        let sdp = "a=rtpmap:96 AppleLossless\r\na=fmtp:96 4000000000 0 16 40 10 14 2 255 0 0 44100\r\n";
        let Err(err) = parse_sdp(sdp) else { panic!("a 4-billion-frame packet was taken") };
        assert!(format!("{err:#}").contains("more than 4096"), "{err:#}");
    }

    #[tokio::test]
    async fn a_header_line_is_cut_off_at_the_budget() {
        let endless = format!("OPTIONS * RTSP/1.0\r\nX: {}", "a".repeat(MAX_HEADER_BYTES));
        let Err(err) = read_request(&mut endless.as_bytes()).await else { panic!("an endless header was taken") };
        assert!(format!("{err:#}").contains("ran past"), "{err:#}");

        let request = read_request(&mut &b"\r\nOPTIONS * RTSP/1.0\r\nCSeq: 3\r\n\r\n"[..]).await.unwrap().unwrap();
        assert_eq!((request.method.as_str(), request.header("CSeq")), ("OPTIONS", Some("3")));
    }

    #[test]
    fn transport_ports_are_found() {
        let t = "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002";
        assert_eq!(transport_port(t, "timing_port"), Some(6002));
        assert_eq!(transport_port(t, "control_port"), Some(6001));
        assert_eq!(transport_port(t, "server_port"), None);
    }
}
