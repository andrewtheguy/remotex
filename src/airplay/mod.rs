//! A Mac's sound, received as an AirPlay 1 speaker.
//!
//! Apple's Screen Sharing carries no audio a client can take in either subtype, so
//! a Mac sends its sound the way it sends it to any speaker: it picks this gateway
//! from its Sound output menu, and streams the system mix over RAOP — RTSP to set
//! up, ALAC over RTP in AES-128-CBC to play. The gateway advertises itself over
//! mDNS as a speaker named after its branding, asks for the password in
//! `[airplay]`, and decodes what arrives into the audio bridge of whatever Apple
//! session with `audio = true` is running. Nothing is ever sent back but
//! answers: it is a sink.
//!
//! The receiver is gateway-wide and outlives sessions, because a Mac selects an
//! AirPlay speaker once and keeps it; a speaker that came and went with each
//! session would have to be picked again every time. While no Apple audio session
//! is running, a stream that arrives is received and thrown away.
//!
//! See docs/airplay-audio.md.

mod crypto;
mod rtp;
mod rtsp;

use std::net::{Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, Weak};

use anyhow::Context as _;
use log::{info, warn};
use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::audio::AudioBridge;
use crate::config::AirPlayConfig;

/// The DNS-SD service type an AirPlay 1 audio receiver registers.
const SERVICE_TYPE: &str = "_raop._tcp.local.";

/// The gateway's AirPlay speaker, running from [`AirPlay::start`] until it is dropped.
pub struct AirPlay {
    shared: Arc<Shared>,
    port: u16,
    /// Kept for the advertisement's sake: dropping the daemon withdraws it.
    _mdns: Option<ServiceDaemon>,
}

/// What every connection's task needs to reach.
struct Shared {
    password: String,
    /// The locally administered address the service name and the Apple-Challenge
    /// answer carry; see [`hw_addr`].
    hw_addr: [u8; 6],
    route: Route,
    /// The one connection currently streaming. A second sender is refused until it
    /// ends, as the gateway has one session to play it to.
    streaming: Mutex<Option<u64>>,
}

/// Where decoded audio goes: the running Apple session's bridge, held weakly so
/// that the session ending is all it takes to stop feeding it.
#[derive(Default)]
struct Route(Mutex<Weak<AudioBridge>>);

impl Route {
    fn current(&self) -> Option<Arc<AudioBridge>> {
        self.0.lock().unwrap().upgrade()
    }
}

impl AirPlay {
    /// Listen, and advertise the speaker on every interface the host has.
    pub fn start(config: &AirPlayConfig) -> anyhow::Result<Arc<Self>> {
        let airplay = Self::listen(config)?;
        let mdns = advertise(config, airplay.port, airplay.shared.hw_addr)?;
        Ok(Arc::new(Self { _mdns: Some(mdns), ..airplay }))
    }

    /// Listen without advertising: the tests' receiver, reached by its port.
    #[cfg(test)]
    fn start_unadvertised(config: &AirPlayConfig) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self::listen(config)?))
    }

    fn listen(config: &AirPlayConfig) -> anyhow::Result<Self> {
        // One dual-stack socket rather than two: a Mac reaches the speaker on
        // whichever address mDNS gave it, and the port is whatever the OS picks,
        // since the advertisement is what carries it. Dual-stack explicitly, since
        // Windows defaults an IPv6 socket to IPv6 only.
        let socket = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, None)
            .context("creating the AirPlay listening socket")?;
        socket.set_only_v6(false).context("making the AirPlay socket dual-stack")?;
        socket
            .bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)).into())
            .context("binding the AirPlay port")?;
        socket.listen(16).context("listening on the AirPlay port")?;
        socket.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(socket.into())
            .context("handing the AirPlay socket to the runtime")?;
        let port = listener.local_addr()?.port();

        let shared = Arc::new(Shared {
            password: config.password.clone(),
            hw_addr: hw_addr(&config.name),
            route: Route::default(),
            streaming: Mutex::new(None),
        });
        let accepting = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut next_id = 0u64;
            loop {
                match listener.accept().await {
                    Ok((tcp, _)) => {
                        next_id += 1;
                        tokio::spawn(rtsp::serve(tcp, next_id, Arc::clone(&accepting)));
                    }
                    Err(e) => warn!("airplay: accepting a sender failed: {e}"),
                }
            }
        });
        Ok(Self { shared, port, _mdns: None })
    }

    /// Send what the speaker receives to `bridge` from now on, until the session
    /// that owns it drops it or another one is attached.
    pub fn attach(&self, bridge: &Arc<AudioBridge>) {
        *self.shared.route.0.lock().unwrap() = Arc::downgrade(bridge);
    }

    /// The RTSP port, which the mDNS record carries.
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// A 48-bit address for the speaker, derived from its name so that two gateways
/// with different names advertise different services, and locally administered
/// (`0x02` set, `0x01` clear) so that it can be no NIC's.
fn hw_addr(name: &str) -> [u8; 6] {
    let hash = xxhash_rust::xxh3::xxh3_64(name.as_bytes()).to_be_bytes();
    [hash[0] & 0xfe | 0x02, hash[1], hash[2], hash[3], hash[4], hash[5]]
}

/// Register `_raop._tcp` as `<hw addr>@<name>`, the instance name AirPlay 1 uses.
fn advertise(config: &AirPlayConfig, port: u16, hw_addr: [u8; 6]) -> anyhow::Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new().context("starting the mDNS responder for AirPlay")?;
    let hw: String = hw_addr.iter().map(|b| format!("{b:02X}")).collect();
    let instance = format!("{hw}@{}", config.name);
    // shairport-sync's classic record without metadata: a password-protected
    // AirPlay 1 speaker taking ALAC or 16-bit PCM at 44.1 kHz, with its AES key
    // RSA-wrapped or not at all.
    let txt: &[(&str, &str)] = &[
        ("txtvers", "1"),
        ("ch", "2"),
        ("cn", "0,1"),
        ("et", "0,1"),
        ("ek", "1"),
        ("sv", "false"),
        ("da", "true"),
        ("sr", "44100"),
        ("ss", "16"),
        ("pw", "true"),
        ("vn", "65537"),
        ("tp", "UDP"),
        ("vs", "105.1"),
        ("am", "remotex"),
        ("sf", "0x4"),
    ];
    let host = format!("{}-airplay.local.", hw.to_lowercase());
    let service = ServiceInfo::new(SERVICE_TYPE, &instance, &host, (), port, txt)
        .context("describing the AirPlay service")?
        .enable_addr_auto();
    mdns.register(service).context("advertising the AirPlay service")?;
    info!("airplay: advertising the speaker {:?} on port {port}", config.name);
    Ok(mdns)
}

#[cfg(test)]
mod tests;
