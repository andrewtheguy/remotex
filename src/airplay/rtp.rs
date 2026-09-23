//! The three UDP ports a SETUP opens: audio (RTP), control (sync and
//! retransmits) and timing (an NTP-like clock exchange).
//!
//! A packet is decoded the moment it arrives and its samples queued for the
//! session's bridge; the sender's playout timestamps are not waited for, since a
//! remote desktop wants its sound now. Nothing is asked for again: a lost packet
//! is a gap, which is what the rest of the gateway's audio does with loss.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use log::{debug, info, warn};
use tokio::net::UdpSocket;
use tokio::sync::watch;

use super::Shared;
use super::crypto::{decrypt_payload, payload_cipher};
use crate::audio::{AudioBridge, PCM_CD_QUALITY};

/// Bytes of PCM gathered before a wave buffer goes to the bridge: three ALAC
/// packets of 352 frames, 24 ms. One packet is 8 ms, and the bridge's queue is
/// sized in buffers, so buffers this small would let a listener fall behind by
/// less than a scheduler hiccup before losing any.
const WAVE_BYTES: usize = 3 * 352 * PCM_CD_QUALITY.block_align() as usize;

/// How often the receiver asks the sender's clock, as shairport-sync does.
const TIMING_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub(super) enum Codec {
    Alac(alac::StreamInfo),
    /// `L16/44100/2`: big-endian 16-bit PCM.
    L16,
}

#[derive(Clone)]
pub(super) struct Params {
    pub codec: Codec,
    /// The AES key and IV, or `None` when the sender chose no encryption.
    pub aes: Option<([u8; 16], [u8; 16])>,
}

/// One stream's sockets and the tasks reading them, which end when this is dropped.
pub(super) struct Stream {
    stop: watch::Sender<bool>,
    pub audio_port: u16,
    pub control_port: u16,
    pub timing_port: u16,
}

impl Stream {
    pub async fn start(
        local: SocketAddr,
        peer: SocketAddr,
        peer_timing_port: Option<u16>,
        params: Params,
        shared: Arc<Shared>,
        session: u64,
    ) -> anyhow::Result<Self> {
        let bind = async |what: &str| {
            UdpSocket::bind(with_port(local, 0))
                .await
                .with_context(|| format!("binding the AirPlay {what} port on {local}"))
        };
        let audio = bind("audio").await?;
        let control = bind("control").await?;
        let timing = bind("timing").await?;
        let (stop, stopped) = watch::channel(false);
        let stream = Self {
            audio_port: audio.local_addr()?.port(),
            control_port: control.local_addr()?.port(),
            timing_port: timing.local_addr()?.port(),
            stop,
        };
        tokio::spawn(audio_loop(audio, peer.ip().to_canonical(), params, shared, session, stopped.clone()));
        tokio::spawn(control_loop(control, stopped.clone()));
        tokio::spawn(timing_loop(timing, peer_timing_port.map(|p| with_port(peer, p)), stopped));
        Ok(stream)
    }
}

/// `addr` on `port`: an IPv4-mapped address as IPv4, as the dual-stack RTSP
/// socket reports an IPv4 sender, and an IPv6 one with its scope kept.
fn with_port(addr: SocketAddr, port: u16) -> SocketAddr {
    match addr.ip().to_canonical() {
        IpAddr::V4(v4) => SocketAddr::new(v4.into(), port),
        IpAddr::V6(_) => {
            let mut addr = addr;
            addr.set_port(port);
            addr
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

/// Receive from `socket` until `stopped` changes, which is the stream ending.
async fn recv<'a>(
    socket: &UdpSocket,
    buf: &'a mut [u8],
    stopped: &mut watch::Receiver<bool>,
) -> Option<(&'a mut [u8], SocketAddr)> {
    loop {
        tokio::select! {
            received = socket.recv_from(buf) => match received {
                Ok((n, from)) => return Some((&mut buf[..n], from)),
                Err(e) => warn!("airplay: receiving on a UDP port failed: {e}"),
            },
            _ = stopped.changed() => return None,
        }
    }
}

/// Play what `peer`, the sender whose RTSP set this stream up, sends to `socket`.
/// Anyone else on the LAN reaching the port is ignored, since the password was
/// asked of `peer` alone.
async fn audio_loop(
    socket: UdpSocket,
    peer: IpAddr,
    params: Params,
    shared: Arc<Shared>,
    session: u64,
    mut stopped: watch::Receiver<bool>,
) {
    let cipher = params.aes.map(|(key, iv)| (payload_cipher(&key), iv));
    let (mut decoder, mut out) = match &params.codec {
        Codec::Alac(info) => (
            Some(alac::Decoder::new(info.clone())),
            vec![0i16; info.max_samples_per_packet() as usize],
        ),
        Codec::L16 => (None, Vec::new()),
    };
    let mut buf = [0u8; 2048];
    let mut wave = Vec::with_capacity(WAVE_BYTES);
    // The bridge this stream last fed, so a session starting mid-stream is told
    // the format before its first buffer, and the one fed is told when it ends.
    let mut fed: Weak<AudioBridge> = Weak::new();
    let (mut packets, mut undecodable) = (0u64, 0u64);

    while let Some((packet, from)) = recv(&socket, &mut buf, &mut stopped).await {
        if from.ip().to_canonical() != peer {
            debug!("airplay: {} bytes on the audio port from {from}, not the sender", packet.len());
            continue;
        }
        if packet.len() < 12 || packet[1] & 0x7f != 0x60 {
            debug!("airplay: {} bytes of type {:#x} on the audio port from {from}", packet.len(), packet.get(1).unwrap_or(&0));
            continue;
        }
        if packets == 0 {
            info!("airplay: audio is arriving from {from}");
        }
        packets += 1;
        let payload = &mut packet[12..];
        if let Some((cipher, iv)) = &cipher {
            decrypt_payload(cipher, iv, payload);
        }
        match (&mut decoder, &params.codec) {
            (Some(decoder), _) => match decoder.decode_packet(payload, &mut out) {
                Ok(samples) => wave.extend(samples.iter().flat_map(|s| s.to_le_bytes())),
                Err(e) => {
                    undecodable += 1;
                    if undecodable.is_power_of_two() {
                        warn!("airplay: {undecodable} ALAC packet(s) would not decode, the last: {e:?}");
                    }
                }
            },
            (None, _) => wave.extend(
                payload
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|b| i16::from_be_bytes(*b).to_le_bytes()),
            ),
        }
        if wave.len() >= WAVE_BYTES {
            feed(&shared, session, &mut fed, std::mem::take(&mut wave));
        }
    }
    if !wave.is_empty() {
        feed(&shared, session, &mut fed, wave);
    }
    if let Some(bridge) = fed.upgrade() {
        bridge.clear_format();
    }
    info!("airplay: the stream ended after {packets} packet(s), {undecodable} undecodable");
}

/// Hand one wave buffer to `session`, the Apple session the stream was set up
/// under, or drop it when that session is no longer the one running.
fn feed(shared: &Shared, session: u64, fed: &mut Weak<AudioBridge>, wave: Vec<u8>) {
    let Some(bridge) = shared.route.of(session) else {
        return;
    };
    if !std::ptr::eq(fed.as_ptr(), Arc::as_ptr(&bridge)) {
        bridge.publish_format(PCM_CD_QUALITY);
        *fed = Arc::downgrade(&bridge);
    }
    bridge.wave(wave);
}

/// Sync packets and retransmits; neither is acted on, so they are only counted.
async fn control_loop(socket: UdpSocket, mut stopped: watch::Receiver<bool>) {
    let mut buf = [0u8; 2048];
    let mut packets = 0u64;
    while let Some((packet, from)) = recv(&socket, &mut buf, &mut stopped).await {
        packets += 1;
        debug!("airplay: {} bytes of type {:#x} on the control port from {from}", packet.len(), packet.get(1).unwrap_or(&0));
    }
    debug!("airplay: {packets} control packet(s)");
}

/// Answer the sender's timing requests and send it ours: some senders hold a
/// stream back until the receiver has taken part in the exchange.
async fn timing_loop(socket: UdpSocket, peer: Option<SocketAddr>, mut stopped: watch::Receiver<bool>) {
    let mut buf = [0u8; 128];
    let mut ask = tokio::time::interval(TIMING_INTERVAL);
    loop {
        let (packet, from) = tokio::select! {
            _ = ask.tick(), if peer.is_some() => {
                let peer = peer.expect("guarded");
                let mut request = [0u8; 32];
                request[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
                request[24..].copy_from_slice(&ntp_now().to_be_bytes());
                if let Err(e) = socket.send_to(&request, peer).await {
                    debug!("airplay: a timing request to {peer} failed: {e}");
                }
                continue;
            }
            received = recv(&socket, &mut buf, &mut stopped) => match received {
                Some(received) => received,
                None => return,
            },
        };
        if packet.len() < 32 || packet[1] & 0x7f != 0x52 {
            continue;
        }
        let now = ntp_now().to_be_bytes();
        let mut reply = [0u8; 32];
        reply[..2].copy_from_slice(&[0x80, 0xd3]);
        reply[2..4].copy_from_slice(&packet[2..4]);
        reply[8..16].copy_from_slice(&packet[24..32]);
        reply[16..24].copy_from_slice(&now);
        reply[24..32].copy_from_slice(&now);
        if let Err(e) = socket.send_to(&reply, from).await {
            debug!("airplay: a timing reply to {from} failed: {e}");
        }
    }
}

/// NTP time: seconds since 1900 in the high word, the fraction in the low one.
fn ntp_now() -> u64 {
    let since_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_unix.as_secs() + 2_208_988_800;
    let frac = (u64::from(since_unix.subsec_nanos()) << 32) / 1_000_000_000;
    (secs << 32) | frac
}
