//! The receiver against a scripted sender, over real sockets: the Digest
//! challenge, an RSA-wrapped key, SETUP, then encrypted ALAC over RTP. ALAC is
//! lossless, so what reaches the session's bridge must be what was sent, sample
//! for sample.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use aes::cipher::{BlockCipherEncrypt as _, KeyInit as _};
use alac_encoder::{AlacEncoder, FormatDescription};
use base64::Engine as _;
use md5::{Digest as _, Md5};
use rsa::{Oaep, Pkcs1v15Sign};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpStream, UdpSocket};

use super::crypto::{B64, public_key};
use super::*;
use crate::audio::PCM_CD_QUALITY;

const FRAMES: usize = 352;
/// Fourteen wave buffers of three packets: within the bridge's queue, so the test
/// can read them all after sending.
const PACKETS: usize = 42;
const PASSWORD: &str = "correct horse";

fn config() -> AirPlayConfig {
    AirPlayConfig { name: "remotex test".into(), password: PASSWORD.into() }
}

struct Sender {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    cseq: u32,
    authorization: Option<(String, String)>,
}

impl Sender {
    async fn connect(port: u16) -> Self {
        // IPv4 on purpose: the listener is one IPv6 socket, and a Mac on a v4
        // address reaches it as a mapped one.
        let tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        let (reader, writer) = tcp.into_split();
        Self { reader: BufReader::new(reader), writer, cseq: 0, authorization: None }
    }

    /// Send a request, and return the status and the headers of the answer.
    async fn request(&mut self, method: &str, headers: &[(&str, &str)], body: &str) -> (u16, Vec<(String, String)>) {
        self.cseq += 1;
        let uri = "rtsp://127.0.0.1/1";
        let mut out = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        if let Some((password, nonce)) = &self.authorization {
            let hex = |text: String| Md5::digest(text).iter().map(|b| format!("{b:02x}")).collect::<String>();
            let ha1 = hex(format!("iTunes:raop:{password}"));
            let ha2 = hex(format!("{method}:{uri}"));
            let response = hex(format!("{ha1}:{nonce}:{ha2}"));
            out.push_str(&format!(
                "Authorization: Digest username=\"iTunes\", realm=\"raop\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\"\r\n"
            ));
        }
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() {
            out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        out.push_str("\r\n");
        out.push_str(body);
        self.writer.write_all(out.as_bytes()).await.unwrap();

        let mut status = String::new();
        self.reader.read_line(&mut status).await.unwrap();
        let code = status.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).await.unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (k, v) = line.split_once(':').unwrap();
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
        assert_eq!(header(&headers, "CSeq"), self.cseq.to_string(), "{method} echoed its CSeq");
        (code, headers)
    }

    /// Answer the Digest challenge with `password`, as a Mac does once asked.
    async fn log_in(&mut self, password: &str) -> u16 {
        let (code, headers) = self.request("OPTIONS", &[], "").await;
        assert_eq!(code, 401, "nothing is answered before the password");
        let challenge = header(&headers, "WWW-Authenticate");
        let nonce = challenge.split("nonce=\"").nth(1).unwrap().trim_end_matches('"').to_owned();
        assert!(challenge.starts_with("Digest realm=\"raop\""), "{challenge}");
        self.authorization = Some((password.to_owned(), nonce));
        let (code, _) = self.request("OPTIONS", &[], "").await;
        code
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("no {name} in {headers:?}"))
}

fn server_port(headers: &[(String, String)]) -> u16 {
    header(headers, "Transport")
        .split(';')
        .find_map(|p| p.strip_prefix("server_port="))
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn a_sender_is_challenged_answered_and_played_to_the_session() {
    let airplay = AirPlay::start_unadvertised(&config()).unwrap();
    let bridge = Arc::new(AudioBridge::new());
    let mut listener = bridge.take_listener();
    airplay.attach(&bridge);

    // The Apple-Challenge is answered even on a refusal, and signs 127.0.0.1 —
    // the IPv4 address, not the mapped IPv6 one — and the advertised address.
    let mut sender = Sender::connect(airplay.port()).await;
    let challenge = [0x5au8; 16];
    let (code, headers) = sender.request("OPTIONS", &[("Apple-Challenge", &B64.encode(challenge))], "").await;
    assert_eq!(code, 401);
    let mut signed = challenge.to_vec();
    signed.extend_from_slice(&[127, 0, 0, 1]);
    signed.extend_from_slice(&hw_addr("remotex test", Path::new("remotex.toml")));
    signed.resize(32, 0);
    public_key()
        .verify(Pkcs1v15Sign::new_unprefixed(), &signed, &B64.decode(header(&headers, "Apple-Response")).unwrap())
        .unwrap();

    // A wrong password is refused, on a fresh connection as a Mac retries.
    let mut wrong = Sender::connect(airplay.port()).await;
    assert_eq!(wrong.log_in("battery staple").await, 401);
    assert_eq!(sender.log_in(PASSWORD).await, 200);

    // ANNOUNCE a session key only the receiver can unwrap.
    let aes_key = *b"remotex-airplay!";
    let aes_iv = *b"0123456789abcdef";
    let wrapped = public_key().encrypt(&mut rand::rng(), Oaep::<sha1::Sha1>::new(), &aes_key).unwrap();
    let sdp = format!(
        "v=0\r\no=iTunes 1 0 IN IP4 127.0.0.1\r\ns=iTunes\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
         a=fmtp:96 {FRAMES} 0 16 40 10 14 2 255 0 0 44100\r\n\
         a=rsaaeskey:{}\r\na=aesiv:{}\r\na=min-latency:11025\r\n",
        B64.encode(wrapped),
        B64.encode(aes_iv),
    );
    assert_eq!(sender.request("ANNOUNCE", &[("Content-Type", "application/sdp")], &sdp).await.0, 200);

    let control = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let timing = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let transport = format!(
        "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
        control.local_addr().unwrap().port(),
        timing.local_addr().unwrap().port()
    );
    let (code, setup) = sender.request("SETUP", &[("Transport", &transport)], "").await;
    assert_eq!(code, 200);
    let audio_port = server_port(&setup);

    // A second sender is turned away while this one streams.
    let mut second = Sender::connect(airplay.port()).await;
    assert_eq!(second.log_in(PASSWORD).await, 200);
    assert_eq!(second.request("ANNOUNCE", &[], &sdp).await.0, 200);
    assert_eq!(second.request("SETUP", &[("Transport", &transport)], "").await.0, 453);

    assert_eq!(sender.request("RECORD", &[("RTP-Info", "seq=0;rtptime=0")], "").await.0, 200);

    // The receiver takes part in the clock exchange.
    let mut request = [0u8; 64];
    let (n, _) = tokio::time::timeout(Duration::from_secs(5), timing.recv_from(&mut request))
        .await
        .expect("a timing request")
        .unwrap();
    assert_eq!((n, request[1]), (32, 0xd2));

    let sent: Vec<i16> = (0..PACKETS * FRAMES)
        .flat_map(|i| {
            let t = i as f32 / 44_100.0;
            let left = (t * 440.0 * std::f32::consts::TAU).sin() * 12_000.0;
            let right = (t * 660.0 * std::f32::consts::TAU).sin() * 8_000.0;
            [left as i16, right as i16]
        })
        .collect();
    let input = FormatDescription::pcm::<i16>(44_100.0, 2);
    let alac = FormatDescription::alac(44_100.0, FRAMES as u32, 2);
    let mut encoder = AlacEncoder::new(&alac);
    let mut packet = vec![0u8; alac.max_packet_size()];
    let cipher = aes::Aes128::new(&aes_key.into());
    let rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let to = SocketAddr::from((Ipv4Addr::LOCALHOST, audio_port));
    for (seq, chunk) in sent.chunks(FRAMES * 2).enumerate() {
        let pcm: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
        let len = encoder.encode(&input, &pcm, &mut packet);
        let mut payload = packet[..len].to_vec();
        let mut chain = aes_iv;
        for block in payload.as_chunks_mut::<16>().0 {
            for (b, c) in block.iter_mut().zip(chain) {
                *b ^= c;
            }
            cipher.encrypt_block(block.into());
            chain = *block;
        }
        let mut datagram = vec![0x80, if seq == 0 { 0xe0 } else { 0x60 }];
        datagram.extend_from_slice(&(seq as u16).to_be_bytes());
        datagram.extend_from_slice(&((seq * FRAMES) as u32).to_be_bytes());
        datagram.extend_from_slice(&0x1234_5678u32.to_be_bytes());
        datagram.extend_from_slice(&payload);
        rtp.send_to(&datagram, to).await.unwrap();
        // Loopback drops a burst; pace it the way a sender does, only faster.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let want = sent.len() * 2;
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.len() < want {
            match listener.queued_wave() {
                Some(wave) => received.extend_from_slice(&wave),
                None => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .expect("every sample reaches the bridge");
    let received: Vec<i16> = received.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
    assert!(received == sent, "the bridge holds exactly what was sent");
    assert_eq!(bridge.negotiated_format(), Some(PCM_CD_QUALITY));

    // TEARDOWN ends the stream, which tells the bridge its source is gone.
    assert_eq!(sender.request("TEARDOWN", &[], "").await.0, 200);
    tokio::time::timeout(Duration::from_secs(5), async {
        while bridge.negotiated_format().is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the format is cleared when the stream ends");
}
