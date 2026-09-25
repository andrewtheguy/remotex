//! A Mac's sound, received as an AirPlay 1 speaker.
//!
//! Current remotex does not receive audio from either Apple Screen Sharing
//! subtype. High Performance has a measured private AAC-ELD-over-SRTP media path,
//! but the Apple engine deliberately does not implement it; Standard has no
//! measured equivalent. AirPlay is the workaround: a Mac sends its sound the way
//! it sends it to any speaker, by picking this gateway from its Sound output menu
//! and streaming the system mix over RAOP — RTSP to set up, ALAC over RTP in
//! AES-128-CBC to play. The gateway advertises itself over mDNS as a speaker named
//! after its branding, asks for the password in `[airplay]`, and decodes what
//! arrives into the audio bridge of whatever Apple session with `audio = true` is
//! running. Nothing is ever sent back but answers: it is a sink.
//!
//! The receiver is gateway-wide and outlives sessions, but a stream does not: a
//! Mac's stream plays into the one Apple audio session that was running when it
//! was set up, and the speaker hangs up on it when that session ends, so the Mac
//! takes its sound back to its own output. While no such session is running, a
//! stream is refused.
//!
//! See docs/airplay-audio.md.

mod crypto;
mod rtp;
mod rtsp;

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::Context as _;
use log::{debug, info, warn};
use mdns_sd::{DaemonEvent, IfKind, IfPredicate, RecvTimeoutError, ServiceDaemon, ServiceInfo};
use tokio::sync::watch;

use crate::audio::AudioBridge;
use crate::config::AirPlayConfig;

/// The DNS-SD service type an AirPlay 1 audio receiver registers.
const SERVICE_TYPE: &str = "_raop._tcp.local.";

/// How long a sender's connection may be idle before it is probed, how often it
/// is probed, and how many probes go unanswered before it is dropped: a sender
/// that is gone releases the stream about a minute later.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_RETRIES: u32 = 3;

/// How long the listener waits after a failed accept before the next.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

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
    /// The Apple audio session the speaker plays into now, by the number
    /// [`AirPlay::attach`] gave it, or `None` between sessions. A stream belongs to
    /// the session it was set up under, and its connection watches this to hang up
    /// when that session ends.
    session: watch::Sender<Option<u64>>,
    next_session: AtomicU64,
    /// The one connection currently streaming. A second sender is refused until it
    /// ends, as the gateway has one session to play it to.
    streaming: Mutex<Option<u64>>,
}

/// Where decoded audio goes: the running Apple session's bridge, with the number
/// [`AirPlay::attach`] gave that session, held weakly so that the session ending
/// is all it takes to stop feeding it.
#[derive(Default)]
struct Route(Mutex<Option<(u64, Weak<AudioBridge>)>>);

impl Route {
    /// The bridge of `session`, if that is still the one running: a stream plays
    /// into the session it was set up under, and into no session that follows.
    fn of(&self, session: u64) -> Option<Arc<AudioBridge>> {
        match &*self.0.lock().unwrap() {
            Some((id, bridge)) if *id == session => bridge.upgrade(),
            _ => None,
        }
    }
}

impl AirPlay {
    /// Listen, and advertise the speaker on every interface the host has.
    /// `config_path` is the gateway's config file, which tells this gateway's
    /// speaker from another's on the same host; see [`hw_addr`].
    pub fn start(config: &AirPlayConfig, config_path: &Path) -> anyhow::Result<Arc<Self>> {
        let airplay = Self::listen(config, config_path)?;
        let mdns = advertise(config, airplay.port, airplay.shared.hw_addr)?;
        Ok(Arc::new(Self { _mdns: Some(mdns), ..airplay }))
    }

    /// Listen without advertising: the tests' receiver, reached by its port.
    #[cfg(test)]
    pub(crate) fn start_unadvertised(config: &AirPlayConfig) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self::listen(config, Path::new("remotex.toml"))?))
    }

    fn listen(config: &AirPlayConfig, config_path: &Path) -> anyhow::Result<Self> {
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
            hw_addr: hw_addr(&config.name, config_path),
            route: Route::default(),
            session: watch::Sender::new(None),
            next_session: AtomicU64::new(0),
            streaming: Mutex::new(None),
        });
        let accepting = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut next_id = 0u64;
            loop {
                match listener.accept().await {
                    Ok((tcp, _)) => {
                        // A Mac that vanishes without a TEARDOWN — asleep, off the
                        // network — would otherwise hold the one stream forever,
                        // and every other sender would be refused.
                        let keepalive = socket2::TcpKeepalive::new()
                            .with_time(KEEPALIVE_IDLE)
                            .with_interval(KEEPALIVE_INTERVAL)
                            .with_retries(KEEPALIVE_RETRIES);
                        if let Err(e) = socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive) {
                            warn!("airplay: enabling keepalive on a sender's connection failed: {e}");
                        }
                        next_id += 1;
                        tokio::spawn(rtsp::serve(tcp, next_id, Arc::clone(&accepting)));
                    }
                    // Paced, so that one that keeps failing — out of file
                    // descriptors — does not spin.
                    Err(e) => {
                        warn!("airplay: accepting a sender failed: {e}");
                        tokio::time::sleep(ACCEPT_RETRY).await;
                    }
                }
            }
        });
        Ok(Self { shared, port, _mdns: None })
    }

    /// Send what the speaker receives to `bridge` from now on, until the returned
    /// guard is dropped — which is the session ending, and hangs up on the Mac
    /// streaming into it — or another session is attached.
    #[must_use = "dropping the guard ends the session's sound"]
    pub fn attach(&self, bridge: &Arc<AudioBridge>) -> Attached {
        let id = self.shared.next_session.fetch_add(1, Ordering::Relaxed) + 1;
        let mut route = self.shared.route.0.lock().unwrap();
        *route = Some((id, Arc::downgrade(bridge)));
        self.shared.session.send_replace(Some(id));
        Attached { shared: Arc::clone(&self.shared), id }
    }

    /// The bridge a stream would be played into now, for the session's tests.
    #[cfg(test)]
    pub(crate) fn attached(&self) -> Option<Arc<AudioBridge>> {
        let route = self.shared.route.0.lock().unwrap();
        route.as_ref().and_then(|(_, bridge)| bridge.upgrade())
    }

    /// The RTSP port, which the mDNS record carries.
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// An Apple audio session's hold on the speaker, from [`AirPlay::attach`]: the
/// engine slot keeps it, so every way an engine ends drops it.
pub struct Attached {
    shared: Arc<Shared>,
    id: u64,
}

impl Drop for Attached {
    fn drop(&mut self) {
        let mut route = self.shared.route.0.lock().unwrap();
        // A newer session attached since is left alone.
        let ended = self.shared.session.send_if_modified(|session| {
            let ours = *session == Some(self.id);
            if ours {
                *session = None;
            }
            ours
        });
        if ended {
            *route = None;
        }
    }
}

/// A 48-bit address for the speaker, derived from its name, the host's and the
/// gateway's config file so that two gateways advertise different services even
/// when they share a branding or a host, yet keep theirs across restarts; and
/// locally administered (`0x02` set, `0x01` clear) so that it can be no NIC's.
fn hw_addr(name: &str, config_path: &Path) -> [u8; 6] {
    let config_path = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_owned());
    let identity = format!(
        "{name}\0{}\0{}",
        gethostname::gethostname().to_string_lossy(),
        config_path.to_string_lossy()
    );
    let hash = xxhash_rust::xxh3::xxh3_64(identity.as_bytes()).to_be_bytes();
    [hash[0] & 0xfe | 0x02, hash[1], hash[2], hash[3], hash[4], hash[5]]
}

/// Register `_raop._tcp` as `<hw addr>@<name>`, the instance name AirPlay 1 uses.
fn advertise(config: &AirPlayConfig, port: u16, hw_addr: [u8; 6]) -> anyhow::Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new().context("starting the mDNS responder for AirPlay")?;
    // mdns-sd answers on an interface with every address in that interface's
    // subnet, and every link-local address is in fe80::/64: a host with many
    // interfaces, such as a Kubernetes node with a veth per pod, would hand the
    // Mac a link-local address for each, scoped to the Mac's own link, where only
    // one of them answers. The Mac picks one and cannot connect. The routable
    // addresses reach it without them.
    mdns.disable_interface(IfKind::Predicate(IfPredicate::new(|intf| match intf.ip() {
        IpAddr::V6(ip) => ip.is_unicast_link_local(),
        IpAddr::V4(_) => false,
    })))
    .context("leaving link-local addresses out of the AirPlay advertisement")?;
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
    // Before registering, so the first announcement is seen.
    watch_advertisement(&mdns, config.name.clone())?;
    let service = ServiceInfo::new(SERVICE_TYPE, &instance, &host, (), port, txt)
        .context("describing the AirPlay service")?
        .enable_addr_auto();
    mdns.register(service).context("advertising the AirPlay service")?;
    info!("airplay: advertising the speaker {:?} on port {port}", config.name);
    Ok(mdns)
}

/// How long the speaker may go unannounced before the log says no Mac can see it.
const ANNOUNCE_DEADLINE: Duration = Duration::from_secs(5);

/// Log what the mDNS responder does with the advertisement from its own thread.
/// It opens its sockets and sends lazily, after `register` has returned, so a
/// failure there — no multicast-capable interface, a refused bind — shows as no
/// announcement and is said so here, not as an error from `start`.
fn watch_advertisement(mdns: &ServiceDaemon, name: String) -> anyhow::Result<()> {
    let events = mdns.monitor().context("watching the mDNS responder for AirPlay")?;
    std::thread::Builder::new()
        .name("airplay-mdns".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + ANNOUNCE_DEADLINE;
            let mut settled = false;
            loop {
                // Waits for the first announcement until the deadline, and then
                // for whatever comes, until the daemon ends with the speaker.
                let event = if settled {
                    events.recv().map_err(|_| RecvTimeoutError::Disconnected)
                } else {
                    events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                };
                match event {
                    Ok(DaemonEvent::Announce(service, on)) => {
                        debug!("airplay: announced {service} on {on}");
                        settled = true;
                    }
                    Ok(DaemonEvent::Error(e)) => warn!("airplay: the mDNS responder failed: {e}"),
                    Ok(DaemonEvent::NameChange(change)) => {
                        info!("airplay: mDNS renamed {} to {}", change.original, change.new_name)
                    }
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        warn!(
                            "airplay: the speaker {name:?} has not been announced after {}s, and no \
                             Mac will see it until it is — check the host has a multicast-capable \
                             interface",
                            ANNOUNCE_DEADLINE.as_secs()
                        );
                        // Said once; an announcement later is still logged.
                        settled = true;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .context("starting the AirPlay mDNS watcher")?;
    Ok(())
}

#[cfg(test)]
mod tests;
