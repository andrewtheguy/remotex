//! What a Windows host does to its graphics pipeline for a slow client, and what a
//! session that passes the pipeline shows it of one.
//!
//! A host sends smaller frames to a client it finds slow. This probe is one passed
//! session over the same moving picture on the host, with one thing made slow at
//! a time, and it prints what the host drew for it.
//!
//! ```sh
//! REMOTEX_UAT_TARGET=windows-ent-sandbox REMOTEX_UAT_MOTION=tmp/link-probe/motion.ps1 \
//!   REMOTEX_UAT_PAGE_KBPS=8000 \
//!   cargo test --profile qa --test rdp_link_probe -- --ignored --nocapture
//! ```
//!
//! [`MOTION_ENV`] names a script run with `start` once the desktop is up and
//! `stop` at the end, to put the same moving picture on the host each time; without
//! it, play something there first.
//!
//! - [`PAGE_ENV`] is a page on a link of that many kbit/s behind a gateway: each
//!   frame is said to be composed when the bytes passed so far would have crossed
//!   that link. This is the case the session is built for: it leaves the host's
//!   socket unread while frames wait, and the host finds the page slow from that.
//! - [`ACK_DELAY_ENV`] says each frame is composed that many milliseconds late, as
//!   a page that far away would, and says that distance to the session, which
//!   holds each round trip of the host's network detection for it.
//! - [`THROTTLE_ENV`] makes the link to the host itself slow: `4000:100` puts a
//!   relay in front of the host that reads from it at 4 Mbit/s, through a small
//!   receive buffer so the host's own socket backs up, and holds everything sent
//!   to it for 100 ms; a third number is that buffer in kilobytes, and a fourth
//!   how many seconds in the relay stops limiting, to see whether the host goes
//!   back.
//! - [`H264_ENV`] lets the host draw with H.264: `1` where it chooses, by region
//!   beside the other codecs, and `whole` preferring it for the whole desktop
//!   (`AVC_THINCLIENT`, [MS-RDPEGFX] 2.2.3.6).
//! - [`AUTODETECT_ENV`] takes part in the host's network detection ([MS-RDPBCGR]
//!   2.2.14), answering with what this connection measures, so behind
//!   [`THROTTLE_ENV`] the host is told of the slow link as well as finding it. Run
//!   with `RUST_LOG=info,remotex::rdp_client=debug` to see what was measured and
//!   what the host concluded.
//! - [`MULTITRANSPORT_ENV`] claims reliable UDP with Soft-Sync ([MS-RDPBCGR]
//!   2.2.1.3.8) and turns down every offer of it the host makes (2.2.15), so the
//!   session stays on TCP: whether a host draws differently for a client that
//!   offers UDP, apart from using it. `RUST_LOG=info` shows the offers.
//!
//! What is printed, for the opening paint and then each [`WINDOW`], is how many
//! frames the host drew and the bytes of their commands by codec; what its
//! RemoteFX Progressive regions say of themselves ([MS-RDPEGFX] 2.2.4.2.1.5), where
//! a host that has found the client slow sends its first passes at a lower quality;
//! the quantization parameter and quality its H.264 regions state (2.2.4.4.2), and
//! of AVC444 which views each frame carried (2.2.4.5); and how far behind the desktop
//! the frames were composed. Nothing but a drawing host is asserted: the rest is
//! the measurement.

mod common;

use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

use remotex::rdp_client::proto::gfx::{self, H264, Message};
use remotex::rdp_client::proto::wire::{Malformed, Reader};
use remotex::rdp_client::{Connect, Event, Session};

const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";
const H264_ENV: &str = "REMOTEX_UAT_H264";
const AUTODETECT_ENV: &str = "REMOTEX_UAT_AUTODETECT";
const MULTITRANSPORT_ENV: &str = "REMOTEX_UAT_MULTITRANSPORT";
const MOTION_ENV: &str = "REMOTEX_UAT_MOTION";
const RUN_ENV: &str = "REMOTEX_UAT_LINK_SECS";
const THROTTLE_ENV: &str = "REMOTEX_UAT_THROTTLE";
const ACK_DELAY_ENV: &str = "REMOTEX_UAT_ACK_DELAY_MS";
const PAGE_ENV: &str = "REMOTEX_UAT_PAGE_KBPS";

const SIZE: (u32, u32) = (1280, 800);

/// How long each printed line covers.
const WINDOW: Duration = Duration::from_secs(5);

/// A relay to `host:port` that is a slow link: what the host sends is read at
/// `kbps` through a small receive buffer, and what is sent to the host is held for
/// `delay`. Returns the local port it listens on, for one connection.
async fn throttle(host: &str, port: u16, kbps: u64, delay: Duration, buffer_kb: u32, lift: Option<Duration>) -> u16 {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let bare = host.trim_start_matches('[').trim_end_matches(']').to_owned();
    let address = tokio::net::lookup_host((bare, port))
        .await
        .expect("resolving the host")
        .next()
        .expect("an address for the host");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("binding the relay");
    let local = listener.local_addr().expect("the relay's address").port();
    tokio::spawn(async move {
        let (client, _) = listener.accept().await.expect("accepting the probe");
        let socket = match address {
            std::net::SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
            std::net::SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
        }
        .expect("a socket");
        // Small, so a host that sends faster than this reads finds its own socket full.
        socket.set_recv_buffer_size(buffer_kb * 1024).expect("a small receive buffer");
        let opened = tokio::time::Instant::now();
        let upstream = socket.connect(address).await.expect("connecting to the host");
        client.set_nodelay(true).expect("nodelay");
        upstream.set_nodelay(true).expect("nodelay");
        let (mut from_client, mut to_client) = client.into_split();
        let (mut from_host, mut to_host) = upstream.into_split();
        let (held, mut due) = tokio::sync::mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 16 * 1024];
            while let Ok(n @ 1..) = from_client.read(&mut buffer).await {
                if held.send((tokio::time::Instant::now() + delay, buffer[..n].to_vec())).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some((at, bytes)) = due.recv().await {
                tokio::time::sleep_until(at).await;
                if to_host.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        // Paced by a clock that owes for every byte, so a sleep that overruns is
        // made up rather than lost.
        let mut buffer = vec![0u8; 1024];
        let mut free = tokio::time::Instant::now();
        while let Ok(n @ 1..) = from_host.read(&mut buffer).await {
            if to_client.write_all(&buffer[..n]).await.is_err() {
                break;
            }
            if lift.is_some_and(|lift| opened.elapsed() >= lift) {
                continue;
            }
            free = free.max(tokio::time::Instant::now() - Duration::from_millis(20))
                + Duration::from_micros(n as u64 * 8000 / kbps);
            tokio::time::sleep_until(free).await;
        }
    });
    local
}

/// Run the motion script, if one was named.
fn motion(action: &str) {
    let Ok(script) = std::env::var(MOTION_ENV) else { return };
    let status = Command::new("pwsh")
        .args(["-NoLogo", "-NoProfile", "-File", &script, action])
        .status()
        .expect("running pwsh");
    assert!(status.success(), "{script} {action} failed: {status}");
}

/// Milliseconds in a day, which the host's frame clock wraps at.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A StartFrame's timestamp as milliseconds of the host's UTC day ([MS-RDPEGFX]
/// 2.2.2.11): milliseconds, seconds, minutes and hours. Zero says the host has no
/// time to give, and gives no sample.
fn time_of_day(timestamp: u32) -> Option<i64> {
    if timestamp == 0 {
        return None;
    }
    let field = |shift: u32, bits: u32| i64::from((timestamp >> shift) & ((1 << bits) - 1));
    Some(field(0, 10) + 1000 * (field(10, 6) + 60 * (field(16, 6) + 60 * field(22, 10))))
}

/// How far behind the desktop a frame was composed, from its offset between the two
/// clocks and the smallest offset seen: taken around the day, so a frame the host
/// began after its midnight is not a day behind one it began before.
fn lag(offset: i64, nearest: i64) -> i64 {
    (offset - nearest).rem_euclid(DAY_MS)
}

/// What one stretch of the session carried.
#[derive(Default)]
struct Tally {
    frames: u64,
    bytes: u64,
    /// The bytes of pixel data, by codec.
    codecs: BTreeMap<u16, u64>,
    /// What the Progressive regions said of themselves.
    progressive: Progressive,
    /// What the H.264 regions said of themselves.
    avc: Avc,
    /// The host's clock at the last StartFrame, in milliseconds of its day.
    began: Option<i64>,
    /// For each frame, the page's clock when it was composed less the host's when
    /// it began, in milliseconds: an offset between two clocks, whose spread is
    /// how far behind the desktop the page ran.
    behind: Vec<i64>,
}

/// What RemoteFX Progressive regions state ([MS-RDPEGFX] 2.2.4.2.1.5): how each
/// tile was sent, the quality of the stage a progressive tile was sent at, and
/// the quantization tables.
#[derive(Default)]
struct Progressive {
    /// Tiles sent whole, as a first pass, and as an upgrade.
    simple: u64,
    first: u64,
    upgrade: u64,
    /// Progressive tiles by the `quality` of their stage; 255 is the full table.
    qualities: BTreeMap<u8, u64>,
    /// Regions by the `quantVals` they carry.
    quants: BTreeMap<Vec<u8>, u64>,
}

/// What H.264 regions state ([MS-RDPEGFX] 2.2.4.4.1, 2.2.4.4.2): the
/// quantization parameter and the quality level the host encoded each at, and of
/// AVC444 (2.2.4.5) which views each frame carried.
#[derive(Default)]
struct Avc {
    /// AVC444 frames by what they carried: luma and chroma, luma alone, chroma alone.
    views: BTreeMap<&'static str, u64>,
    /// Regions by their `qp`.
    quantizers: BTreeMap<u8, u64>,
    /// Regions by their `qualityVal`.
    qualities: BTreeMap<u8, u64>,
}

impl Avc {
    fn add(&mut self, data: &[u8]) -> Result<(), Malformed> {
        let mut r = Reader::new("an AVC420 metablock", data);
        let rects = usize::try_from(r.u32_le()?).unwrap_or(usize::MAX);
        r.skip(rects.saturating_mul(8))?;
        for said in r.bytes(rects.saturating_mul(2))?.chunks(2) {
            *self.quantizers.entry(said[0] & 0x3F).or_default() += 1;
            *self.qualities.entry(said[1]).or_default() += 1;
        }
        Ok(())
    }

    /// An AVC444 or AVC444v2 bitmap stream: the first stream's metablock is read as
    /// AVC420's is, and the views the stream says it carries are counted.
    fn add_444(&mut self, data: &[u8]) -> Result<(), Malformed> {
        let mut r = Reader::new("an AVC444 bitmap stream", data);
        let said = r.u32_le()?;
        let views = match said >> 30 {
            0 => "luma and chroma",
            1 => "luma",
            2 => "chroma",
            _ => "invalid",
        };
        *self.views.entry(views).or_default() += 1;
        let first = usize::try_from(said & 0x3FFF_FFFF).unwrap_or(usize::MAX);
        self.add(r.bytes(first)?)
    }

    fn line(&self) -> String {
        format!("h264 qp {:?}; quality {:?}; avc444 views {:?}", self.quantizers, self.qualities, self.views)
    }

    fn merge(&mut self, other: &Avc) {
        for (views, frames) in &other.views {
            *self.views.entry(views).or_default() += frames;
        }
        for (qp, regions) in &other.quantizers {
            *self.quantizers.entry(*qp).or_default() += regions;
        }
        for (quality, regions) in &other.qualities {
            *self.qualities.entry(*quality).or_default() += regions;
        }
    }
}

impl Progressive {
    fn add(&mut self, data: &[u8]) -> Result<(), Malformed> {
        const WHAT: &str = "a Progressive block";
        let mut r = Reader::new(WHAT, data);
        while !r.is_empty() {
            let kind = r.u16_le()?;
            let length = usize::try_from(r.u32_le()?).unwrap_or(usize::MAX);
            let body = r.bytes(length.saturating_sub(6))?;
            if kind != 0xCCC4 {
                continue;
            }
            let mut region = Reader::new(WHAT, body);
            region.u8()?; // tileSize
            let rects = usize::from(region.u16_le()?);
            let quants = usize::from(region.u8()?);
            let stages = usize::from(region.u8()?);
            region.u8()?; // flags
            region.u16_le()?; // numTiles
            region.u32_le()?; // tileDataSize
            region.skip(rects * 8)?;
            *self.quants.entry(region.bytes(quants * 5)?.to_vec()).or_default() += 1;
            let stages: Vec<u8> = region.bytes(stages * 16)?.chunks(16).map(|stage| stage[0]).collect();
            while !region.is_empty() {
                let kind = region.u16_le()?;
                let length = usize::try_from(region.u32_le()?).unwrap_or(usize::MAX);
                let tile = region.bytes(length.saturating_sub(6))?;
                // Past the three quantization indexes and the tile's place: a first
                // pass has its flags before the stage, an upgrade does not.
                let stage = match kind {
                    0xCCC5 => {
                        self.simple += 1;
                        continue;
                    }
                    0xCCC6 => {
                        self.first += 1;
                        tile.get(8)
                    }
                    0xCCC7 => {
                        self.upgrade += 1;
                        tile.get(7)
                    }
                    _ => continue,
                };
                let quality = stage.map_or(255, |index| stages.get(usize::from(*index)).copied().unwrap_or(255));
                *self.qualities.entry(quality).or_default() += 1;
            }
        }
        Ok(())
    }

    fn line(&self) -> String {
        let quants: Vec<String> = self
            .quants
            .iter()
            .map(|(table, regions)| format!("{}x{regions}", table.iter().map(|b| format!("{b:02x}")).collect::<String>()))
            .collect();
        format!(
            "tiles {} whole, {} first, {} upgrade; stage qualities {:?}; quantVals {}",
            self.simple,
            self.first,
            self.upgrade,
            self.qualities,
            quants.join(" "),
        )
    }

    fn merge(&mut self, other: &Progressive) {
        self.simple += other.simple;
        self.first += other.first;
        self.upgrade += other.upgrade;
        for (quality, tiles) in &other.qualities {
            *self.qualities.entry(*quality).or_default() += tiles;
        }
        for (table, regions) in &other.quants {
            *self.quants.entry(table.clone()).or_default() += regions;
        }
    }
}

impl Tally {
    fn add(&mut self, commands: &[u8], frame: bool) {
        self.frames += u64::from(frame);
        self.bytes += commands.len() as u64;
        for message in gfx::messages(commands) {
            let (codec, data) = match message.expect("the passed commands parse") {
                // [MS-RDPEGFX] 2.2.2.11: milliseconds, seconds, minutes, hours.
                Message::StartFrame { timestamp, .. } => {
                    self.began = time_of_day(timestamp);
                    continue;
                }
                Message::WireToSurface1 { codec, data, .. } | Message::WireToSurface2 { codec, data, .. } => {
                    (codec, data)
                }
                _ => continue,
            };
            *self.codecs.entry(codec).or_default() += data.len() as u64;
            if matches!(codec, gfx::CODEC_CAPROGRESSIVE | gfx::CODEC_CAPROGRESSIVE_V2) {
                self.progressive.add(data).expect("the Progressive blocks parse");
            }
            if codec == gfx::CODEC_AVC420 {
                self.avc.add(data).expect("the AVC420 metablock parses");
            }
            if matches!(codec, gfx::CODEC_AVC444 | gfx::CODEC_AVC444_V2) {
                self.avc.add_444(data).expect("the AVC444 bitmap stream parses");
            }
        }
    }

    fn line(&self, over: Duration) -> String {
        let secs = over.as_secs_f64();
        let codecs: Vec<String> = self
            .codecs
            .iter()
            .map(|(codec, bytes)| format!("{} {:.0}", gfx::codec_name(*codec), *bytes as f64 * 8.0 / 1000.0 / secs))
            .collect();
        format!(
            "{:5.1} frames/s, {:6.0} kbit/s, {:6.0} bytes/frame  [{} kbit/s]",
            self.frames as f64 / secs,
            self.bytes as f64 * 8.0 / 1000.0 / secs,
            self.bytes as f64 / self.frames.max(1) as f64,
            codecs.join(", "),
        )
    }

    fn merge(&mut self, other: &Tally) {
        self.frames += other.frames;
        self.bytes += other.bytes;
        for (codec, bytes) in &other.codecs {
            *self.codecs.entry(*codec).or_default() += bytes;
        }
        self.progressive.merge(&other.progressive);
        self.avc.merge(&other.avc);
    }
}

#[tokio::test]
#[ignore = "drives a real RDP host named in tmp/test_uat.toml, with a moving picture on its desktop"]
async fn a_real_host_draws_for_a_slow_client() {
    common::init_logging();
    let name = std::env::var(TARGET_ENV)
        .unwrap_or_else(|_| panic!("set {TARGET_ENV} to the name of an rdp target in tmp/test_uat.toml"));
    let target = common::uat_target(&name);
    let h264 = match std::env::var(H264_ENV).as_deref() {
        Err(_) | Ok("" | "0" | "false") => H264::Refused,
        Ok("whole") => H264::WholeDesktop,
        Ok(_) => H264::ByRegion,
    };
    let autodetect = matches!(std::env::var(AUTODETECT_ENV).as_deref(), Ok(v) if !matches!(v, "" | "0" | "false"));
    let multitransport =
        matches!(std::env::var(MULTITRANSPORT_ENV).as_deref(), Ok(v) if !matches!(v, "" | "0" | "false"));
    let run = Duration::from_secs(std::env::var(RUN_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(60));
    let ack_delay = Duration::from_millis(std::env::var(ACK_DELAY_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    let throttled = std::env::var(THROTTLE_ENV).ok().map(|said| {
        let mut parts = said.split(':');
        let mut number = |otherwise: u64| parts.next().map_or(otherwise, |part| part.parse::<u64>().expect("a number"));
        let (kbps, delay, buffer_kb, lift) = (number(0), number(0), number(16), number(0));
        (kbps, Duration::from_millis(delay), buffer_kb as u32, (lift > 0).then(|| Duration::from_secs(lift)))
    });
    println!(
        "rdp_link_probe: {name}, h264 {h264:?}, autodetect {autodetect}, udp offered {multitransport}, throttle {throttled:?}, acks held {ack_delay:?}, {}s",
        run.as_secs()
    );
    let (host, port) = match throttled {
        Some((kbps, delay, buffer_kb, lift)) => {
            ("127.0.0.1".to_owned(), throttle(&target.host, target.port, kbps, delay, buffer_kb, lift).await)
        }
        None => (target.host.clone(), target.port),
    };
    // Acknowledgements, each sent once it has been held.
    let (acks, mut held) = tokio::sync::mpsc::unbounded_channel::<(tokio::time::Instant, u32)>();

    let (session, mut events) = Session::start(Connect {
        host,
        port,
        username: target.username.clone(),
        password: target.password.clone(),
        domain: target.domain.clone(),
        width: SIZE.0,
        height: SIZE.1,
        scale_percent: 0,
        monitors: 1,
        placement: remotex::config::Placement::Right,
        resize: false,
        egfx: true,
        h264,
        autodetect,
        multitransport,
        audio: None,
        camera: None,
        microphone: None,
    });
    let acknowledger = session.input().clone();
    tokio::spawn(async move {
        while let Some((at, frame)) = held.recv().await {
            tokio::time::sleep_until(at).await;
            acknowledger.frame_composed(frame);
        }
    });
    // When the page's link is free of everything passed so far.
    let page_kbps: Option<u64> = std::env::var(PAGE_ENV).ok().map(|said| said.parse().expect("kbit/s"));
    let mut link_free = tokio::time::Instant::now();
    let epoch = tokio::time::Instant::now();
    // Returns the page's clock, in milliseconds, when it composes the frame.
    let mut acknowledge = move |frame: Option<u32>, bytes: usize| -> Option<i64> {
        let now = tokio::time::Instant::now();
        if let Some(kbps) = page_kbps {
            link_free = link_free.max(now) + Duration::from_micros(bytes as u64 * 8000 / kbps);
        }
        let composed = link_free.max(now) + ack_delay;
        let _ = acks.send((composed, frame?));
        Some(composed.duration_since(epoch).as_millis() as i64)
    };
    // The smallest offset seen between the two clocks: a frame that waited for
    // nothing.
    let mut nearest = i64::MAX;
    let first = tokio::time::timeout(Duration::from_secs(90), events.recv())
        .await
        .expect("no first event within 90s")
        .expect("the event channel closed");
    let Event::Connected { width, height } = first else {
        panic!("the session did not connect: {first:?}");
    };
    println!("  connected at {width}x{height}");
    // A page that late to compose is that far away, and is said to be: once
    // connected, because what is said before the desktop exists is dropped.
    if !ack_delay.is_zero() {
        session.input().caller_distance(ack_delay);
    }

    // The desktop first, so the picture starts on a session that is drawing.
    let mut opening = Tally::default();
    let settled = Instant::now() + Duration::from_secs(8);
    while let Ok(event) = tokio::time::timeout_at(settled.into(), events.recv()).await {
        match event.expect("the event channel closed without an Ended") {
            Event::Graphics { commands, frame } => {
                opening.add(&commands, frame.is_some());
                if let (Some(composed), Some(began)) = (acknowledge(frame, commands.len()), opening.began) {
                    nearest = nearest.min(composed - began);
                }
            }
            Event::Ended(result) => panic!("the session ended: {result:?}"),
            _ => {}
        }
    }
    assert!(opening.frames > 0, "the host drew nothing");
    println!("  opening {}", opening.line(Duration::from_secs(8)));
    println!("         {}", opening.progressive.line());
    println!("         {}", opening.avc.line());
    motion("start");

    let started = Instant::now();
    let mut total = Tally::default();
    let mut window = Tally::default();
    let mut window_started = started;
    // How much of the run the total covers.
    let mut covered = Duration::ZERO;
    let mut windows = 0;
    while windows < run.as_secs() / WINDOW.as_secs() {
        let due = window_started + WINDOW;
        match tokio::time::timeout_at(due.into(), events.recv()).await {
            Ok(event) => match event.expect("the event channel closed without an Ended") {
                Event::Graphics { commands, frame } => {
                    window.add(&commands, frame.is_some());
                    if let (Some(composed), Some(began)) = (acknowledge(frame, commands.len()), window.began) {
                        nearest = nearest.min(composed - began);
                        window.behind.push(composed - began);
                    }
                }
                Event::Ended(result) => panic!("the session ended: {result:?}"),
                _ => {}
            },
            Err(_) => {
                println!("  {:5.1}s {}", started.elapsed().as_secs_f32(), window.line(window_started.elapsed()));
                println!("         {}", window.progressive.line());
                println!("         {}", window.avc.line());
                let mut behind: Vec<i64> = window.behind.iter().map(|&offset| lag(offset, nearest)).collect();
                behind.sort_unstable();
                if let (Some(median), Some(most)) = (behind.get(behind.len() / 2), behind.last()) {
                    println!("         behind the desktop: median {median} ms, most {most} ms");
                }
                // The first window is the picture starting, which is not the picture.
                if windows > 0 {
                    total.merge(&window);
                    covered += window_started.elapsed();
                }
                windows += 1;
                window = Tally::default();
                window_started = Instant::now();
            }
        }
    }
    motion("stop");
    println!("  after the first {}s: {}", WINDOW.as_secs(), total.line(covered));
    assert!(total.frames > 0, "the host drew nothing once the picture was moving");
    drop(session);
}

/// A StartFrame timestamp, packed as 2.2.2.11 lays it out.
fn packed(hours: u32, minutes: u32, seconds: u32, millis: u32) -> u32 {
    (hours << 22) | (minutes << 16) | (seconds << 10) | millis
}

#[test]
fn a_frame_with_no_timestamp_gives_no_sample() {
    assert_eq!(time_of_day(0), None);
}

#[test]
fn a_timestamp_reads_as_milliseconds_of_the_day() {
    assert_eq!(time_of_day(packed(13, 45, 7, 250)), Some(((13 * 60 + 45) * 60 + 7) * 1000 + 250));
    assert_eq!(time_of_day(packed(0, 0, 0, 1)), Some(1), "a millisecond past midnight is a time");
}

/// Two frames composed 20 ms apart on the page's clock, the host beginning the
/// first a millisecond before its midnight and the second 19 ms after it: both
/// equally behind, rather than the second a day behind.
#[test]
fn the_lag_is_taken_around_the_hosts_midnight() {
    let before = time_of_day(packed(23, 59, 59, 999)).unwrap();
    let after = time_of_day(packed(0, 0, 0, 19)).unwrap();
    let first = 5_000 - before;
    let second = 5_020 - after;
    let nearest = first.min(second);
    assert_eq!(lag(first, nearest), 0);
    assert_eq!(lag(second, nearest), 0);
    assert_eq!(lag(first + 30, nearest), 30, "a later composition is behind by what it waited");
}
