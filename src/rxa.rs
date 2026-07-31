//! Gateway side of RXA. Agent-encoded PNG/JPEG tiles pass through unchanged.
//! Established authenticated links reconnect with a bounded retry window;
//! initial connection and authentication failures are reported immediately.

use std::time::Duration;

use log::{debug, info, warn};
use rxa_proto::frame::{FrameReader, FrameWriter};
use rxa_proto::msg::{AgentMsg, GatewayMsg};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};

use crate::config::TargetConfig;
use crate::engine::{self, clamp_u16, host_port};
use crate::protocol::{
    ClientMsg, ClipboardSnapshot, CursorShape, DisplayInfo, MouseButton, ServerMsg, Tile, WheelUnit,
    clipboard_fits,
};

/// How long a connect + handshake + `Hello` may take before we give up on this
/// attempt. Guards against a host that accepts the TCP connection and then says
/// nothing, which no socket timeout would catch.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect backoff bounds for an established session that dropped.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// Maximum continuous outage before reporting failure. It includes several
/// capped-backoff attempts and remains below the session reattach grace.
const RECONNECT_GIVE_UP: Duration = Duration::from_secs(30);

/// The reconnect policy, as a value so tests can shrink the clock.
///
/// Injected rather than read from the constants directly for the same reason
/// [`crate::ws`] injects its heartbeat timings — and here it is the only option:
/// each engine runs on its own thread with its own current-thread runtime (see
/// [`crate::session`]), so `tokio::time::pause` in a test cannot reach this
/// timer, and a test on the real constants would take half a minute.
#[derive(Clone, Copy)]
struct Retry {
    backoff_min: Duration,
    backoff_max: Duration,
    give_up_after: Duration,
}

const RETRY: Retry = Retry {
    backoff_min: BACKOFF_MIN,
    backoff_max: BACKOFF_MAX,
    give_up_after: RECONNECT_GIVE_UP,
};

/// Application-level keepalive, on top of the socket's own.
///
/// [`crate::engine`] arms `SO_KEEPALIVE` with a roughly 25-second budget, which
/// is what the *kernel* can promise: that the agent's host is still answering.
/// This ping is both faster and different in kind — it is answered by the agent
/// process, so it also catches a Mac that is reachable while the agent is wedged,
/// which is the failure keepalive is blind to.
///
/// It is also why this engine's own socket keepalive effectively never arms: a
/// ping every five seconds means the connection is never idle, and idle is the
/// only state the kernel probes in.
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// A link with no `Pong` for this long is treated as dead and reconnected.
const PONG_TIMEOUT: Duration = Duration::from_secs(15);

/// Buffer of decoded agent messages between the reader task and the pump.
/// Bounded so a slow browser backpressures through to the agent's TCP window
/// instead of queueing frames here.
const AGENT_BUFFER: usize = 32;

/// The state that outlives one agent link.
///
/// `announced`, `displays` and `clipboard` survive a silent reconnect for the same
/// reason: the *browser* did not go anywhere. What it has been told about the
/// desktop, which displays its menu lists, and when it last saw the remote
/// clipboard change are all still true on the other side of a dropped TCP
/// connection.
///
/// `relayed` is the exception, and lives here only to be carried between the
/// pumps rather than across them: every new link starts it from nothing
/// ([`RelayMemo::forget`] at the top of [`pump`]). A browser that attached during
/// the outage had its `Refresh` discarded with the rest of the stale input, so
/// nothing else would tell the memo that canvas is now blank.
#[derive(Default)]
struct Carried {
    /// What the browser has been told about the desktop. A `Resize` costs the
    /// frontend its canvas contents, so a silent reconnect to an unchanged display
    /// must not announce one.
    announced: Option<Announced>,
    /// The agent's last display list — the whole of what a client's display menu
    /// knows. A browser attaching in the gap between a reconnect and the agent's
    /// next report would otherwise find no menu at all.
    displays: Option<(u32, Vec<DisplayInfo>)>,
    /// The agent supplies the clipboard activity time, so holding the snapshot is
    /// what stops an otherwise-identical Fetch losing its timestamp merely because
    /// the transport link changed underneath it.
    clipboard: Option<ClipboardSnapshot>,
    /// See [`RelayMemo`].
    relayed: RelayMemo,
}

/// Digest of the last deterministic payload for each rectangle. This suppresses
/// duplicate coarse damage without decoding or retaining a second framebuffer.
#[derive(Default)]
struct RelayMemo {
    sent: std::collections::HashMap<(u16, u16, u16, u16), u64>,
    dropped: u64,
    dropped_bytes: u64,
}

impl RelayMemo {
    /// Whether this payload is new for its rectangle, recording it either way.
    fn is_new(&mut self, rect: (u16, u16, u16, u16), data: &[u8]) -> bool {
        let digest = xxhash_rust::xxh3::xxh3_64(data);
        if self.sent.insert(rect, digest) == Some(digest) {
            self.dropped += 1;
            self.dropped_bytes += data.len() as u64;
            return false;
        }
        true
    }

    /// Forget everything, because the browser is about to be repainted from
    /// scratch or the coordinate space has changed under it.
    ///
    /// Not merely tidy. The session layer drops frames while no browser is
    /// attached ([`crate::session`]), so without this a reattached browser would
    /// be denied tiles this memo believes it already has — a permanently blank
    /// region. `Refresh` is injected on every attach, which is what makes
    /// clearing it there enough to cover detach, reattach and takeover alike.
    fn forget(&mut self) {
        self.report();
        self.sent.clear();
        self.dropped = 0;
        self.dropped_bytes = 0;
    }

    /// Log what the dedup has saved, if anything. Reported from both places the
    /// count stops being current: a repaint, and the end of the session.
    fn report(&self) {
        if self.dropped > 0 {
            info!(
                "rxa: relay dedup dropped {} repeated tile(s) / {} bytes",
                self.dropped, self.dropped_bytes
            );
        }
    }
}

/// This gateway's session, as the agent's session slot knows it
/// ([`rxa_proto::msg::GatewayMsg::Claim`]).
///
/// One value per **process**, and that is the whole design: a gateway instance
/// has exactly one session slot (see `docs/roadmap.md`), so every dial this
/// process ever makes is the same session by definition. The agent therefore
/// grants all of them without a prompt — a dropped link, a target switched away
/// and back, a browser taking the gateway over, and a half-open connection the
/// agent has not reaped yet are all this same id coming back.
///
/// Deliberately not derived from the keys, the target, or the browser's claim
/// token. The keys are authentication and answer a different question; a
/// per-target id would make switching targets look like a stranger; and the
/// browser's token is minted afresh by a takeover, which would make one person
/// pressing "take over" here demand a second takeover at the Mac.
///
/// Random rather than counted, because it is compared across two machines and
/// only equality means anything. It authenticates nothing — the handshake did
/// that — so it needs no secrecy, only distinctness.
fn session_id() -> [u8; 16] {
    static ID: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    *ID.get_or_init(|| *uuid::Uuid::new_v4().as_bytes())
}

/// Connect to the Mac agent and drive the session until the browser goes away.
///
/// `input_rx` carries browser input; `frame_tx` carries screen updates back.
/// Unlike the other engines this returns only when the *session* ends (the
/// session layer dropped the input channel, or the browser link closed) — a
/// broken agent link is retried, not reported.
///
/// `takeover` answers a [`ServerMsg::RemoteBusy`] the client showed a person: it
/// takes the agent's session slot from whoever holds it. One-shot by
/// construction, because it lives on this call — every reconnect inside the run
/// reclaims on [`session_id`] instead, which needs no force.
pub async fn run(
    config: TargetConfig,
    takeover: bool,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    run_with(config, takeover, RETRY, input_rx, frame_tx).await
}

async fn run_with(
    config: TargetConfig,
    takeover: bool,
    retry: Retry,
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    // Both already validated in `ConfigFile::parse`, so these only fire if a
    // target reached us without going through the config loader.
    let keys = match keys(&config) {
        Ok(keys) => keys,
        Err(e) => {
            warn!("rxa: unusable keys for target {:?}: {e}", config.name);
            let _ = frame_tx.send(ServerMsg::Error { message: e }).await;
            return;
        }
    };

    let mut session = match connect(&config, &keys, takeover).await {
        Ok(session) => session,
        // The Mac is somebody else's. Reported as itself rather than as an error,
        // so the client can offer the one thing that resolves it. `taken_over` is
        // false because this client asked and was refused — it had nothing to lose.
        Err(DialError::Busy { holder, held_secs }) => {
            let _ = frame_tx
                .send(ServerMsg::RemoteBusy {
                    holder,
                    held_secs,
                    taken_over: false,
                })
                .await;
            return;
        }
        Err(DialError::Other(e)) => {
            warn!("rxa: connect failed: {e:#}");
            let _ = frame_tx
                .send(ServerMsg::Error {
                    message: format!("rxa connect failed: {e}"),
                })
                .await;
            return;
        }
    };

    let caps = Caps {
        clipboard: config.clipboard,
        resize: config.resize,
    };
    let mut backoff = retry.backoff_min;
    let mut carried = Carried::default();
    loop {
        let size = (session.width, session.height);
        info!(
            "rxa: session up, desktop {}x{} at {}x",
            size.0, size.1, session.scale
        );
        match pump(
            session,
            &mut input_rx,
            &frame_tx,
            &mut carried,
            caps,
        )
        .await
        {
            Ok(()) => {
                info!("rxa: session ended");
                carried.relayed.report();
                return;
            }
            // The link broke. Everything below is the silent-reconnect path.
            Err(e) => warn!("rxa: link lost: {e:#}"),
        }

        // Restarted per outage, which is what makes a successful reconnect reset
        // the window — see [`RECONNECT_GIVE_UP`].
        let lost_at = Instant::now();
        session = loop {
            if !idle(backoff, &mut input_rx, &frame_tx).await {
                info!("rxa: session ended while reconnecting");
                return;
            }
            // Never forced, however this run began: the slot is ours by
            // [`session_id`] as long as we hold it, and if somebody has taken it
            // from us in the meantime that was a person's decision. Retrying over
            // the top of it would be this gateway overruling them.
            match connect(&config, &keys, false).await {
                Ok(session) => break session,
                // Somebody took the Mac while our link was down. No amount of
                // waiting undoes that, so stop here and let the client offer the
                // takeover rather than spending the rest of the window failing.
                // The other way round from the initial dial: this session was
                // running and somebody else claimed it. `taken_over` is what stops
                // the client telling the person who just lost their desktop that a
                // target they never picked is busy.
                Err(DialError::Busy { holder, held_secs }) => {
                    info!("rxa: giving up the agent link — {holder} took the session over");
                    let _ = frame_tx
                        .send(ServerMsg::RemoteBusy {
                            holder,
                            held_secs,
                            taken_over: true,
                        })
                        .await;
                    return;
                }
                Err(DialError::Other(e)) => {
                    debug!("rxa: reconnect failed, retrying: {e:#}");
                    // Checked after the attempt, so a link that is down for less
                    // than the window always gets at least one try at coming back.
                    if lost_at.elapsed() >= retry.give_up_after {
                        warn!(
                            "rxa: giving up on the agent link after {}s",
                            lost_at.elapsed().as_secs()
                        );
                        let _ = frame_tx
                            .send(ServerMsg::Error {
                                message: format!(
                                    "the Mac agent did not come back within {}s",
                                    retry.give_up_after.as_secs()
                                ),
                            })
                            .await;
                        // Returning drops `frame_tx`, and the session layer turns
                        // that into a `Picker` for the browser (see
                        // [`crate::session`]).
                        return;
                    }
                    backoff = (backoff * 2).min(retry.backoff_max);
                }
            }
        };
        backoff = retry.backoff_min;
    }
}

/// The desktop geometry as the browser last heard it: pixel size and the density
/// those pixels are drawn at. The density belongs here because it changes on its
/// own — the same panel has HiDPI and 1x modes — and an unannounced change would
/// leave the desktop presented at half or twice its size.
type Announced = (u16, u16, f32);

/// What the target's profile opted this session into, as one value.
///
/// One struct rather than two bare `bool` parameters, because both are `bool` and
/// they would sit next to each other: transposing them at a call site compiles,
/// and the transposition writes the Mac's pasteboard for a target that never
/// opted into the clipboard bridge. Named fields make that a type error.
#[derive(Clone, Copy)]
struct Caps {
    /// `clipboard` on the target: the pasteboard bridge in both directions, and
    /// the watcher this engine enables per attach.
    clipboard: bool,
    /// `resize` on the target: whether a client may ask the agent to change the
    /// size of the display it is sharing. Only the operator's half of that — the
    /// agent refuses any display it did not make itself.
    resize: bool,
}

/// An established, handshaken link to the agent.
struct Session {
    reader: FrameReader<OwnedReadHalf>,
    writer: FrameWriter<OwnedWriteHalf>,
    width: u16,
    height: u16,
    /// The shared display's backing scale: 2.0 for a Retina Mac, whose
    /// framebuffer is twice the desktop it shows. Clients divide by it, and it
    /// travels with every size the agent reports.
    scale: f32,
}

/// What this target authenticates with: our own private key and the public key
/// of the one Mac it is paired with.
struct Keys {
    private: [u8; 32],
    agent_public: [u8; 32],
}

/// Parse both halves out of the target, turning either failure into the message
/// the browser is shown.
///
/// `ConfigFile::parse` has already accepted both, so this is the belt to that
/// braces — and the place the two `expect`s it would otherwise take are not.
fn keys(config: &TargetConfig) -> Result<Keys, String> {
    use rxa_proto::key::{Role, parse_private, parse_public};

    let private = parse_private(Role::Gateway, &config.gateway_private_key)
        .map_err(|e| format!("rxa gateway has an invalid [rxa].private_key: {e}"))?;
    let agent_public = parse_public(Role::Agent, &config.agent_public_key)
        .map_err(|e| format!("rxa target has an invalid agent_public_key: {e}"))?;
    Ok(Keys {
        private,
        agent_public,
    })
}

/// Why a dial did not produce a session.
///
/// Two cases and not one, because they deserve opposite treatment: a busy remote
/// is a settled answer that no amount of retrying will change, and everything
/// else is weather.
#[derive(Debug, thiserror::Error)]
enum DialError {
    /// A different session holds the agent's slot and this dial did not ask to
    /// take it over. Only a person can resolve it, so it is reported at once and
    /// never retried.
    #[error("the Mac is in use from {holder}")]
    Busy { holder: String, held_secs: u32 },
    /// The host is down, the keys are wrong, the agent speaks another version —
    /// anything a later attempt might find differently.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// TCP connect → Noise handshake → claim the agent's session slot → read the
/// agent's `Hello`.
///
/// `force` takes the slot from whoever holds it. It belongs to the *dial* rather
/// than to the engine on purpose: only the first attempt of a run carries what a
/// person asked for, and a reconnect must never re-force. Otherwise a client that
/// took the Mac from us fairly would be evicted again by our own retry loop, and
/// two gateways would trade the slot back and forth with nobody choosing.
async fn connect(config: &TargetConfig, keys: &Keys, force: bool) -> Result<Session, DialError> {
    /// What one dial produced. Split from [`DialError`] so the dial itself can go
    /// on using `anyhow` throughout — a refusal is an ordinary answer down here,
    /// and only becomes an error worth distinguishing to the caller.
    enum Dialed {
        Ready(Session),
        Busy { holder: String, held_secs: u32 },
    }

    let dest = host_port(&config.host, config.port);
    let dialed = timeout(CONNECT_TIMEOUT, async {
        let mut stream = engine::tcp_connect(&dest).await?;

        let transport = rxa_proto::noise::initiate(&mut stream, &keys.private, &keys.agent_public)
            .await
            .map_err(|e| anyhow::anyhow!("handshake with {dest}: {e}"))?;
        let (read_half, write_half) = stream.into_split();
        let (mut reader, mut writer) = rxa_proto::frame::split(read_half, write_half, transport);

        // Ours to ask for, not to assume: the agent serves one session at a time
        // and this says which one is asking. It goes out before the agent has said
        // anything, because the agent cannot decide who to evict until it knows.
        writer
            .send(
                &GatewayMsg::Claim {
                    version: rxa_proto::VERSION,
                    session: session_id(),
                    force,
                }
                .encode(),
            )
            .await?;

        // `Hello` is the agent's answer to that claim; anything else means we are
        // not talking to an agent that agrees with us about the protocol.
        let (width, height, scale) = match AgentMsg::decode(&reader.recv().await?)? {
            AgentMsg::Hello {
                version,
                agent_version,
                w,
                h,
                scale,
            } => {
                anyhow::ensure!(
                    version == rxa_proto::VERSION,
                    "agent speaks rxa version {version}, this build speaks {}",
                    rxa_proto::VERSION
                );
                let scale = rxa_proto::msg::scale_ratio(scale);
                info!("rxa: agent {agent_version} at {dest}, screen {w}x{h} at {scale}x");
                (w, h, scale)
            }
            // The other answer to the claim: somebody else has the Mac. Not an
            // error here — the agent behaved correctly and told us who — so it
            // leaves by the same door a session does and is sorted out below.
            AgentMsg::Busy { holder, held_secs } => {
                info!("rxa: {dest} is in use from {holder} ({held_secs}s)");
                return Ok(Dialed::Busy { holder, held_secs });
            }
            // Most likely a missing Screen Recording grant — say what the agent
            // said rather than "unexpected message".
            AgentMsg::Error { message } => anyhow::bail!("agent reported: {message}"),
            other => anyhow::bail!("expected Hello from the agent, got {other:?}"),
        };
        Ok(Dialed::Ready(Session {
            reader,
            writer,
            width,
            height,
            scale,
        }))
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out connecting to {dest}"))??;

    match dialed {
        Dialed::Ready(session) => Ok(session),
        Dialed::Busy { holder, held_secs } => Err(DialError::Busy { holder, held_secs }),
    }
}

/// Drive one established link.
///
/// `Ok(())` means the *session* is over and there is nothing to reconnect for;
/// `Err` means this link broke and the caller should reconnect.
async fn pump(
    session: Session,
    input_rx: &mut mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: &mpsc::Sender<ServerMsg>,
    carried: &mut Carried,
    caps: Caps,
) -> anyhow::Result<()> {
    let Session {
        reader,
        mut writer,
        mut width,
        mut height,
        mut scale,
    } = session;

    // Every link starts the dedup from nothing, including a reconnect to a
    // browser that never went away and still holds every pixel it was sent.
    //
    // Deduping that reconnect repaint would be sound *if* the browser really is
    // the same one — but it might not be. `idle` discards input during an outage
    // (stale input is worse than none), and a browser that attached in that window
    // had its `Refresh` discarded with the rest, so nothing else would tell this
    // memo the canvas is now blank. Re-sending one screen per agent reconnect is
    // cheap and rare; a permanently blank region is neither.
    carried.relayed.forget();

    // Nothing to discover: the agent only builds for macOS, so reaching one at
    // all settles it.
    if frame_tx
        .send(ServerMsg::RemoteOs { macos: true })
        .await
        .is_err()
    {
        return Ok(()); // browser link already gone
    }

    // On the initial connect, and afterwards only when the Mac came back a
    // different size. Unchanged, the browser keeps the canvas it already has and
    // the reconnect stays invisible beyond a pause in frames.
    if carried.announced != Some((width, height, scale)) {
        if frame_tx
            .send(ServerMsg::Resize {
                w: width,
                h: height,
                scale,
            })
            .await
            .is_err()
        {
            return Ok(()); // browser link already gone
        }
        carried.announced = Some((width, height, scale));
    }

    // `FrameReader::recv` is not cancel-safe, so it cannot live in the
    // `select!` below — it gets its own task and forwards decoded messages.
    let (agent_tx, mut agent_rx) = mpsc::channel(AGENT_BUFFER);
    let read_task = tokio::spawn(read_loop(reader, agent_tx));

    // Guarantees the reader task is joined however this function exits — an
    // orphan would hold the socket open across a reconnect.
    let _abort = AbortOnDrop(read_task);

    writer.send(&GatewayMsg::Attach.encode()).await?;

    // Sent per attach, not once per process: this runs again after every
    // reconnect, and the agent's watch state died with the old session. Only
    // for an opted-in target — the agent reads nothing unprompted otherwise.
    if caps.clipboard {
        writer
            .send(&GatewayMsg::ClipboardWatch { enabled: true }.encode())
            .await?;
    }

    let mut ping = interval(PING_INTERVAL);
    // A blocked browser must not cause a burst of catch-up pings.
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut nonce: u64 = 0;
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            agent = agent_rx.recv() => {
                let Some(msg) = agent else {
                    anyhow::bail!("agent link closed");
                };
                last_seen = Instant::now();
                match msg {
                    AgentMsg::Tile { format, x, y, w, h, data } => {
                        // The one payload on this link the gateway cannot inspect, so
                        // the format byte is checked instead of trusted. An
                        // unrecognised value would reach a client that rejects the
                        // *whole batch* it arrives in — a blank region with nothing in
                        // any log — where dropping one tile here costs a rectangle
                        // until the next repaint covers it.
                        //
                        // Unreachable while both halves ship together, which
                        // rxa_proto::VERSION enforces at the handshake. This is for
                        // the case where they do not.
                        if format != rxa_proto::msg::format::PNG
                            && format != rxa_proto::msg::format::JPEG
                        {
                            warn!("rxa: dropping a {w}x{h} tile with unknown format byte {format}");
                            continue;
                        }
                        if !carried.relayed.is_new((x, y, w, h), &data) {
                            continue; // the browser is already showing these pixels
                        }
                        let tile = Tile::encoded(format, x, y, w, h, data);
                        if frame_tx.send(ServerMsg::Tile(tile)).await.is_err() {
                            return Ok(());
                        }
                    }
                    AgentMsg::Cursor(shape) => {
                        let shape = shape.map(|c| CursorShape {
                            w: c.w,
                            h: c.h,
                            hx: c.hx,
                            hy: c.hy,
                            png: c.png,
                        });
                        if frame_tx.send(ServerMsg::Cursor(shape)).await.is_err() {
                            return Ok(());
                        }
                    }
                    AgentMsg::DisplaySize { w, h, scale: reported } => {
                        scale = rxa_proto::msg::scale_ratio(reported);
                        info!("rxa: display reconfigured to {w}x{h} at {scale}x");
                        (width, height) = (w, h);
                        carried.announced = Some((w, h, scale));
                        // The client clears its canvas on a resize, and the same
                        // rectangle now means different pixels.
                        carried.relayed.forget();
                        if frame_tx.send(ServerMsg::Resize { w, h, scale }).await.is_err() {
                            return Ok(());
                        }
                    }
                    // A second Hello on a live link (the agent restarted its
                    // stream) carries the current size; treat it as a resize.
                    AgentMsg::Hello { w, h, scale: reported, .. } => {
                        scale = rxa_proto::msg::scale_ratio(reported);
                        (width, height) = (w, h);
                        carried.announced = Some((w, h, scale));
                        carried.relayed.forget();
                        if frame_tx.send(ServerMsg::Resize { w, h, scale }).await.is_err() {
                            return Ok(());
                        }
                    }
                    // Relayed as it arrives, and kept: a browser that attaches
                    // later gets it from the cache below rather than waiting for
                    // the agent's next change.
                    AgentMsg::Displays { active, displays: reported } => {
                        let reported: Vec<DisplayInfo> = reported
                            .into_iter()
                            .map(|display| DisplayInfo {
                                id: display.id,
                                main: display.is_main(),
                                virtual_display: display.is_owned(),
                                label: display.label,
                                detail: display.detail,
                            })
                            .collect();
                        carried.displays = Some((active, reported.clone()));
                        if frame_tx
                            .send(ServerMsg::Displays { active, displays: reported })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    AgentMsg::Pong { .. } => {}
                    // Either a reply to a ClipboardRequest this pump sent, or
                    // an unprompted push from the agent's pasteboard watcher.
                    // The payload shares one path, but `requested` stays intact
                    // so only watcher pushes drive browser OS-clipboard sync.
                    //
                    // Dropped outright for a target that didn't opt in: this
                    // pump then never asked and never enabled the watch, so
                    // anything arriving is an agent that disagrees with us —
                    // and the browser writes an incoming clipboard into the
                    // real OS clipboard. Same belt-and-braces as VNC's
                    // ServerCutText.
                    AgentMsg::Clipboard {
                        text,
                        changed_at_ms,
                        requested,
                        oversized_bytes,
                    } => {
                        if let Some(bytes) = oversized_bytes {
                            debug!(
                                "rxa: the Mac's pasteboard holds {bytes} bytes, over the {} byte limit",
                                crate::protocol::MAX_CLIPBOARD_BYTES
                            );
                        }
                        let changed_at_ms = changed_at_ms.or_else(|| {
                            carried.clipboard
                                .as_ref()
                                .filter(|snapshot| snapshot.text == text)
                                .and_then(|snapshot| snapshot.changed_at_ms)
                        });
                        let snapshot = ClipboardSnapshot {
                            text,
                            changed_at_ms,
                            oversized_bytes,
                        };
                        carried.clipboard = Some(snapshot.clone());
                        if caps.clipboard
                            && frame_tx
                                .send(ServerMsg::Clipboard {
                                    text: snapshot.text,
                                    changed_at_ms: snapshot.changed_at_ms,
                                    requested,
                                    oversized_bytes: snapshot.oversized_bytes,
                                })
                                .await
                                .is_err()
                        {
                            return Ok(());
                        }
                    }
                    // The agent can't proceed — a missing TCC grant, typically.
                    // Reconnecting would just rediscover it, so this is fatal.
                    AgentMsg::Error { message } => {
                        warn!("rxa: agent error: {message}");
                        let _ = frame_tx
                            .send(ServerMsg::Error {
                                message: format!("Mac agent: {message}"),
                            })
                            .await;
                        return Ok(());
                    }
                    // Only ever an answer to a claim, which was granted before this
                    // pump existed — the link we are reading it on is the proof.
                    // Ignored rather than acted on: telling the browser the Mac is
                    // busy while its desktop is arriving would be a lie.
                    AgentMsg::Busy { holder, held_secs } => {
                        warn!(
                            "rxa: ignoring a Busy({holder}, {held_secs}s) on a session \
                             the agent already granted"
                        );
                    }
                }
            }

            input = input_rx.recv() => {
                let Some(msg) = input else {
                    return Ok(()); // the session layer tore this engine down
                };
                // A reattaching browser has a blank canvas and no idea what the
                // remote runs: re-announce both before asking the agent to
                // repaint.
                if matches!(msg, ClientMsg::Refresh) {
                    // Before anything else: a repaint exists to put pixels on a
                    // canvas that has none, so nothing may be withheld from it.
                    carried.relayed.forget();
                    if frame_tx
                        .send(ServerMsg::Resize { w: width, h: height, scale })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    if frame_tx.send(ServerMsg::RemoteOs { macos: true }).await.is_err() {
                        return Ok(());
                    }
                    // And no display menu. Re-sent from here rather than asked
                    // for, because `Refresh` reaches the agent as a repaint and
                    // a repaint is all it should be: the list has not changed
                    // just because a browser came back to look at it.
                    if let Some((active, list)) = carried.displays.clone()
                        && frame_tx
                            .send(ServerMsg::Displays { active, displays: list })
                            .await
                            .is_err()
                    {
                        return Ok(());
                    }
                }
                if let Some(out) = to_agent(&msg, caps, scale) {
                    writer.send(&out.encode()).await?;
                }
            }

            _ = ping.tick() => {
                if last_seen.elapsed() > PONG_TIMEOUT {
                    anyhow::bail!(
                        "no response from the agent for {}s",
                        last_seen.elapsed().as_secs()
                    );
                }
                nonce += 1;
                writer.send(&GatewayMsg::Ping { nonce }.encode()).await?;
            }
        }
    }
}

/// Read frames off the link, decode them, and forward until either end closes.
async fn read_loop(mut reader: FrameReader<OwnedReadHalf>, tx: mpsc::Sender<AgentMsg>) {
    loop {
        let frame = match reader.recv().await {
            Ok(frame) => frame,
            Err(e) => {
                debug!("rxa: read ended: {e}");
                return;
            }
        };
        let msg = match AgentMsg::decode(&frame) {
            Ok(msg) => msg,
            Err(e) => {
                // Undecodable means the two halves disagree about the wire;
                // dropping the link is the honest response.
                warn!("rxa: undecodable message from the agent: {e}");
                return;
            }
        };
        if tx.send(msg).await.is_err() {
            return; // the pump is gone
        }
    }
}

/// Aborts the reader task on drop, so no `pump` exit path leaks it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Translate client input to RXA. Viewports are gated by target capability and
/// converted from remote pixels to points with the last announced scale; the
/// agent independently restricts resize to its own active display.
fn to_agent(msg: &ClientMsg, caps: Caps, scale: f32) -> Option<GatewayMsg> {
    Some(match msg {
        ClientMsg::MouseMove { x, y } => GatewayMsg::PointerMove {
            x: clamp_u16(*x),
            y: clamp_u16(*y),
        },
        ClientMsg::MouseButton {
            button,
            pressed,
            clicks,
        } => GatewayMsg::PointerButton {
            // DOM `MouseEvent.button` numbering, unchanged — the agent maps it
            // to a `CGMouseButton`.
            button: match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                MouseButton::Back => 3,
                MouseButton::Forward => 4,
            },
            pressed: *pressed,
            // Floored, not trusted: a browser is free to send a zero and the
            // agent turns this straight into a click state.
            clicks: (*clicks).max(1),
        },
        // Raw DOM deltas; the agent owns the sign and unit conversion, because
        // only it can verify the direction against a real trackpad. The unit
        // rides along because the agent cannot infer it: a three-pixel glide and
        // a three-line notch are the same number.
        ClientMsg::Wheel { dx, dy, unit } => GatewayMsg::Wheel {
            dx: *dx,
            dy: *dy,
            unit: match unit {
                WheelUnit::Pixel => rxa_proto::msg::WheelUnit::Pixel,
                WheelUnit::Line => rxa_proto::msg::WheelUnit::Line,
                WheelUnit::Page => rxa_proto::msg::WheelUnit::Page,
            },
        },
        ClientMsg::Key {
            code,
            pressed,
            caps,
        } => GatewayMsg::Key {
            code: code.clone(),
            pressed: *pressed,
            caps: *caps,
        },
        ClientMsg::Refresh => GatewayMsg::Refresh,
        ClientMsg::SelectDisplay { id } => GatewayMsg::SelectDisplay { id: *id },
        // Forwarded whatever display is being shared: only the agent knows
        // whether the current one is a display it made, and only it can act.
        ClientMsg::HostScale { scale } => GatewayMsg::HostScale { scale: *scale },
        // Pixels in, points out. `scale` is guarded rather than trusted: it comes
        // from `scale_ratio`, which already refuses nonsense, but a zero here
        // would divide a window into infinity and the fallback that matters is
        // "assume the two units agree", which is what every 1x target is anyway.
        ClientMsg::Viewport { w, h } if caps.resize => {
            let scale = if scale > 0.0 { scale } else { 1.0 };
            let points = |px: u16| clamp_u16((f32::from(px) / scale).round() as i32);
            GatewayMsg::ResizeDisplay {
                w: points(*w),
                h: points(*h),
            }
        }
        // The one size request that needs no conversion, because it carries no
        // size: the created size is in points already, and it is the agent that
        // knows it. Which is the argument for the message existing rather than
        // this engine substituting a number — nothing here can name the size a
        // display was made at, and the pixels-to-points division above would be
        // the wrong instrument for it anyway.
        ClientMsg::DefaultSize if caps.resize => GatewayMsg::DefaultDisplaySize,
        // The agent reads its pasteboard only when asked, so a fetch is a real
        // round trip rather than a cached value (unlike VNC, where the server
        // pushes and the engine caches).
        ClientMsg::ClipboardRequest if caps.clipboard => GatewayMsg::ClipboardRequest,
        // Refused rather than truncated, so the Mac's pasteboard keeps what it
        // had instead of gaining a partial copy that looks whole. The browser
        // and the viewer both refuse this themselves and say why; reaching here
        // means one of them let it through.
        ClientMsg::Clipboard { text } if caps.clipboard && !clipboard_fits(text) => {
            warn!(
                "rxa: refusing {} bytes to the Mac's pasteboard, over the {} byte limit",
                text.len(),
                crate::protocol::MAX_CLIPBOARD_BYTES
            );
            return None;
        }
        ClientMsg::Clipboard { text } if caps.clipboard => GatewayMsg::Clipboard {
            text: text.clone(),
        },
        // The opted-out cases of the two gates above, plus the pair the session
        // layer never forwards. A `Viewport` or `DefaultSize` reaching here is a
        // target whose profile says no, which is also the only state in which
        // either client withholds the request — so this is the belt, not the
        // braces.
        ClientMsg::ClipboardRequest
        | ClientMsg::Clipboard { .. }
        | ClientMsg::Viewport { .. }
        | ClientMsg::DefaultSize
        | ClientMsg::Connect { .. }
        | ClientMsg::Disconnect
        | ClientMsg::CacheReset
        | ClientMsg::Audio { .. } => {
            return None;
        }
    })
}

/// Wait out a reconnect backoff, discarding input that arrives meanwhile.
///
/// Returns `false` when there is no longer anything to reconnect *for* — the
/// session layer dropped the input channel or output receiver. Input
/// buffered during an outage is deliberately thrown away rather than replayed:
/// a mouse position from eight seconds ago is worse than no event at all, and
/// an undrained unbounded channel would grow for as long as the outage lasts.
async fn idle(
    backoff: Duration,
    input_rx: &mut mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> bool {
    let sleep = tokio::time::sleep(backoff);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return !frame_tx.is_closed(),
            input = input_rx.recv() => {
                if input.is_none() {
                    return false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy with the same shape as [`RETRY`] and a clock a test can wait on.
    ///
    /// The real one gives up after 30 seconds; nothing here should take longer
    /// than a blink. See [`Retry`] for why this is injected rather than paused.
    const FAST_RETRY: Retry = Retry {
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(20),
        give_up_after: Duration::from_millis(100),
    };

    /// A paired gateway and Mac: the target as the gateway reads it, and the
    /// two keys the fake agent needs to answer a dial from it.
    struct Pairing {
        target: TargetConfig,
        agent_private: [u8; 32],
        gateway_public: [u8; 32],
    }

    fn pairing(port: u16) -> Pairing {
        use rxa_proto::key::{Role, generate_private, parse_private, public_of};

        let gateway_private_key = generate_private(Role::Gateway);
        let agent_private_key = generate_private(Role::Agent);
        let agent_private = parse_private(Role::Agent, &agent_private_key).unwrap();
        let gateway_public =
            public_of(&parse_private(Role::Gateway, &gateway_private_key).unwrap());
        // What the config loader would have produced: the agent's public key in
        // the target, this gateway's private key fanned in by `resolve`.
        let target = TargetConfig {
            name: "mac".to_owned(),
            protocol: crate::config::Protocol::Rxa,
            subtype: None,
            host: "127.0.0.1".to_owned(),
            port,
            username: String::new(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: 1,
            height: 1,
            security: crate::config::Security::Auto,
            resize: false,
            clipboard: false,
            audio: false,
            agent_public_key: rxa_proto::key::public_text_of(Role::Agent, &agent_private_key)
                .unwrap(),
            gateway_private_key,
        };
        Pairing {
            target,
            agent_private,
            gateway_public,
        }
    }

    /// Serve one link — handshake, claim, `Hello`, one paint — then hang up,
    /// returning the claim so a caller can check what was asked for.
    ///
    /// The listener is *not* consumed, so the caller decides whether the agent
    /// ever comes back: dropping it is a Mac that was switched off, keeping it is
    /// one whose link merely blipped.
    async fn serve_one_link(
        listener: &tokio::net::TcpListener,
        agent_private: [u8; 32],
        gateway_public: [u8; 32],
    ) -> GatewayMsg {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (transport, ()) = rxa_proto::noise::respond(&mut stream, &agent_private, |k| {
            (*k == gateway_public).then_some(())
        })
        .await
        .unwrap();
        let (read_half, write_half) = stream.into_split();
        let (mut reader, mut writer) = rxa_proto::frame::split(read_half, write_half, transport);
        // The gateway speaks first now, asking for this agent's session slot.
        let claim = GatewayMsg::decode(&reader.recv().await.unwrap()).unwrap();
        assert!(
            matches!(claim, GatewayMsg::Claim { .. }),
            "the first frame must be the claim, got {claim:?}"
        );
        writer
            .send(
                &AgentMsg::Hello {
                    version: rxa_proto::VERSION,
                    agent_version: "fake-agent".to_owned(),
                    w: 800,
                    h: 600,
                    scale: rxa_proto::msg::SCALE_ONE,
                }
                .encode(),
            )
            .await
            .unwrap();
        // The gateway asks for the stream before any pixels flow.
        match GatewayMsg::decode(&reader.recv().await.unwrap()).unwrap() {
            GatewayMsg::Attach => {}
            other => panic!("expected Attach, got {other:?}"),
        }
        claim
    }

    // The bug this bound exists for: a Mac that is switched off was retried
    // forever while the browser held a frozen desktop that claimed to be live.
    #[tokio::test]
    async fn an_agent_that_never_comes_back_is_reported_instead_of_retried_forever() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let paired = pairing(port);
        let (agent_private, gateway_public) = (paired.agent_private, paired.gateway_public);

        let agent = tokio::spawn(async move {
            serve_one_link(&listener, agent_private, gateway_public).await;
            // Everything goes, listener included: nothing will answer again.
        });

        let (_input_tx, input_rx) = mpsc::unbounded_channel();
        let (frame_tx, mut frame_rx) = mpsc::channel(16);
        run_with(paired.target, false, FAST_RETRY, input_rx, frame_tx).await;
        agent.await.unwrap();

        // `run_with` returning at all is half the point — that is what drops
        // `frame_tx` and lands the browser on the picker.
        let mut reported = None;
        while let Ok(msg) = frame_rx.try_recv() {
            if let ServerMsg::Error { message } = msg {
                reported = Some(message);
            }
        }
        let message = reported.expect("the browser was told nothing");
        assert!(message.contains("did not come back"), "{message}");
    }

    // And the behaviour that must survive it: a link that comes back is still a
    // non-event, which is the whole reason the silent retry exists.
    #[tokio::test]
    async fn a_reconnect_that_succeeds_restarts_the_give_up_window() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let paired = pairing(port);
        let (agent_private, gateway_public) = (paired.agent_private, paired.gateway_public);

        let agent = tokio::spawn(async move {
            // Hang up twice, each time well inside the window, then stop
            // answering. A bound measured from the *first* outage rather than
            // per-outage would have given up during this.
            for _ in 0..3 {
                serve_one_link(&listener, agent_private, gateway_public).await;
            }
        });

        let (_input_tx, input_rx) = mpsc::unbounded_channel();
        let (frame_tx, mut frame_rx) = mpsc::channel(16);
        run_with(paired.target, false, FAST_RETRY, input_rx, frame_tx).await;
        agent.await.unwrap();

        // It still ends up reporting — the agent stops answering eventually —
        // but only after all three links were served, which is what says the
        // window restarted rather than running from the first drop.
        let mut errors = 0;
        while let Ok(msg) = frame_rx.try_recv() {
            if matches!(msg, ServerMsg::Error { .. }) {
                errors += 1;
            }
        }
        assert_eq!(errors, 1, "exactly one report, after the last link");
    }

    // A tile whose format byte this build does not know costs that one tile, not
    // the batch it would have travelled in.
    //
    // Worth a test even though `rxa_proto::VERSION` makes it unreachable between a
    // matched pair, because the consequence is invisible: both clients drop an
    // entire frame on an unknown format byte, so the symptom would be a region of
    // stale pixels with nothing in any log to explain it.
    #[tokio::test]
    async fn a_tile_with_an_unknown_format_byte_is_dropped_and_the_next_one_still_arrives() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let paired = pairing(port);
        let (agent_private, gateway_public) = (paired.agent_private, paired.gateway_public);

        let agent = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (transport, ()) = rxa_proto::noise::respond(&mut stream, &agent_private, |k| {
                (*k == gateway_public).then_some(())
            })
            .await
            .unwrap();
            let (read_half, write_half) = stream.into_split();
            let (mut reader, mut writer) = rxa_proto::frame::split(read_half, write_half, transport);
            match GatewayMsg::decode(&reader.recv().await.unwrap()).unwrap() {
                GatewayMsg::Claim { .. } => {}
                other => panic!("expected the claim first, got {other:?}"),
            }
            writer
                .send(
                    &AgentMsg::Hello {
                        version: rxa_proto::VERSION,
                        agent_version: "fake-agent".to_owned(),
                        w: 800,
                        h: 600,
                        scale: rxa_proto::msg::SCALE_ONE,
                    }
                    .encode(),
                )
                .await
                .unwrap();
            match GatewayMsg::decode(&reader.recv().await.unwrap()).unwrap() {
                GatewayMsg::Attach => {}
                other => panic!("expected Attach, got {other:?}"),
            }
            // 3 was WebP before this protocol version, so it is exactly the byte a
            // stale agent would send — now unrecognised and dropped, where both PNG
            // and JPEG (the two the classifier picks between) relay through.
            for (format, data) in [
                (3u8, vec![b'R', b'I', b'F', b'F']),
                (rxa_proto::msg::format::PNG, vec![0x89, b'P', b'N', b'G']),
                (rxa_proto::msg::format::JPEG, vec![0xFF, 0xD8, 0xFF, 0xE0]),
            ] {
                writer
                    .send(
                        &AgentMsg::Tile { format, x: 7, y: 9, w: 4, h: 4, data }.encode(),
                    )
                    .await
                    .unwrap();
            }
            // Hold the link open long enough for both to be pumped.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let (_input_tx, input_rx) = mpsc::unbounded_channel();
        let (frame_tx, mut frame_rx) = mpsc::channel(16);
        run_with(paired.target, false, FAST_RETRY, input_rx, frame_tx).await;
        agent.await.unwrap();

        let tiles: Vec<_> = std::iter::from_fn(|| frame_rx.try_recv().ok())
            .filter_map(|msg| match msg {
                ServerMsg::Tile(tile) => Some(tile),
                _ => None,
            })
            .collect();
        assert_eq!(tiles.len(), 2, "only the unknown format byte should cost a tile");
        assert_eq!(tiles[0].format, Tile::FORMAT_PNG);
        assert_eq!(tiles[0].data, b"\x89PNG");
        assert_eq!(tiles[1].format, Tile::FORMAT_JPEG);
        assert_eq!(tiles[1].data, &[0xFF, 0xD8, 0xFF, 0xE0]);
    }

    /// A target that opted into nothing, which is the default and what most of
    /// the translation below is indifferent to.
    const NO_CAPS: Caps = Caps {
        clipboard: false,
        resize: false,
    };
    const CLIPBOARD: Caps = Caps {
        clipboard: true,
        resize: false,
    };
    const RESIZE: Caps = Caps {
        clipboard: false,
        resize: true,
    };
    /// The density in every case where the conversion is not what is under test.
    /// A 1x remote is the one scale at which viewport pixels and display points
    /// are the same number, so it never hides an arithmetic mistake behind an
    /// identity — which is why the conversion has tests of its own.
    const UNSCALED: f32 = 1.0;

    #[test]
    fn pointer_moves_carry_clamped_framebuffer_coordinates() {
        assert_eq!(
            to_agent(&ClientMsg::MouseMove { x: 1279, y: 799 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::PointerMove { x: 1279, y: 799 })
        );
        // A drag off the canvas edge pins to the edge instead of vanishing.
        assert_eq!(
            to_agent(&ClientMsg::MouseMove { x: -5, y: 70_000 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::PointerMove { x: 0, y: u16::MAX })
        );
    }

    #[test]
    fn buttons_keep_the_dom_numbering() {
        for (button, expected) in [
            (MouseButton::Left, 0),
            (MouseButton::Middle, 1),
            (MouseButton::Right, 2),
        ] {
            assert_eq!(
                to_agent(&ClientMsg::MouseButton {
                    button,
                    pressed: true,
                    clicks: 1
                }, NO_CAPS, UNSCALED),
                Some(GatewayMsg::PointerButton {
                    button: expected,
                    pressed: true,
                    clicks: 1
                })
            );
        }
        assert_eq!(
            to_agent(&ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: false,
                clicks: 1
            }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::PointerButton {
                button: 0,
                pressed: false,
                clicks: 1
            })
        );
    }

    // Back and forward keep the DOM's own numbering, which macOS happens to
    // share — the agent turns 3 and 4 straight into `CGMouseButton`s.
    #[test]
    fn the_side_buttons_reach_the_agent_as_three_and_four() {
        for (button, expected) in [(MouseButton::Back, 3), (MouseButton::Forward, 4)] {
            assert_eq!(
                to_agent(&ClientMsg::MouseButton {
                    button,
                    pressed: true,
                    clicks: 1
                }, NO_CAPS, UNSCALED),
                Some(GatewayMsg::PointerButton {
                    button: expected,
                    pressed: true,
                    clicks: 1
                })
            );
        }
    }

    // The unit is the difference between a three-pixel glide and a three-line
    // notch, so it has to survive the hop rather than be normalised here.
    #[test]
    fn the_wheel_unit_is_carried_rather_than_normalised() {
        for (sent, expected) in [
            (WheelUnit::Pixel, rxa_proto::msg::WheelUnit::Pixel),
            (WheelUnit::Line, rxa_proto::msg::WheelUnit::Line),
            (WheelUnit::Page, rxa_proto::msg::WheelUnit::Page),
        ] {
            assert_eq!(
                to_agent(
                    &ClientMsg::Wheel { dx: 1.0, dy: 2.0, unit: sent },
                    NO_CAPS,
                    UNSCALED
                ),
                Some(GatewayMsg::Wheel {
                    dx: 1.0,
                    dy: 2.0,
                    unit: expected
                }),
                "{sent:?}"
            );
        }
    }

    /// The count the client reports is what the agent must inject: it is the only
    /// end of the link that knows whether the person double-clicked, and macOS
    /// guessing it from arrival times is the bug this carries it for.
    #[test]
    fn the_click_count_is_carried_and_floored_at_one() {
        for (sent, expected) in [(1, 1), (2, 2), (3, 3), (0, 1)] {
            assert_eq!(
                to_agent(&ClientMsg::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                    clicks: sent
                }, NO_CAPS, UNSCALED),
                Some(GatewayMsg::PointerButton {
                    button: 0,
                    pressed: true,
                    clicks: expected
                }),
                "clicks {sent}"
            );
        }
    }

    // Wheel deltas pass through untouched: the agent owns sign and units, so
    // the gateway must not "helpfully" normalize them.
    #[test]
    fn wheel_deltas_pass_through_unmodified() {
        assert_eq!(
            to_agent(
                &ClientMsg::Wheel { dx: 0.0, dy: -2.5, unit: WheelUnit::Line },
                NO_CAPS,
                UNSCALED
            ),
            Some(GatewayMsg::Wheel {
                dx: 0.0,
                dy: -2.5,
                unit: rxa_proto::msg::WheelUnit::Line
            })
        );
    }

    #[test]
    fn keys_carry_the_dom_code_and_the_caps_flag_verbatim() {
        assert_eq!(
            to_agent(&ClientMsg::Key {
                code: "KeyA".to_owned(),
                pressed: true,
                caps: true,
            }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::Key {
                code: "KeyA".to_owned(),
                pressed: true,
                caps: true,
            })
        );
        // Codes this build has no macOS keycode for are still forwarded — the
        // table lives in rxa-proto and the agent is the one that consults it.
        assert_eq!(
            to_agent(&ClientMsg::Key {
                code: "MediaPlayPause".to_owned(),
                pressed: false,
                caps: false,
            }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::Key {
                code: "MediaPlayPause".to_owned(),
                pressed: false,
                caps: false,
            })
        );
    }

    // The client's screen density reaches the agent whatever display is being
    // shared: only the agent knows whether the current one is a display it made,
    // and the gateway holding an opinion here would be a second place for the
    // answer to be wrong.
    #[test]
    fn the_hosts_density_reaches_the_agent() {
        assert_eq!(
            to_agent(&ClientMsg::HostScale { scale: 200 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::HostScale { scale: 200 })
        );
        assert_eq!(
            to_agent(&ClientMsg::HostScale { scale: 100 }, NO_CAPS, UNSCALED),
            Some(GatewayMsg::HostScale { scale: 100 })
        );
    }

    #[test]
    fn refresh_asks_the_agent_for_a_full_repaint() {
        assert_eq!(to_agent(&ClientMsg::Refresh, NO_CAPS, UNSCALED), Some(GatewayMsg::Refresh));
    }

    // The session layer never forwards Connect/Disconnect, and a viewport from a
    // target that did not opt in is dropped rather than acted on — the opted-in
    // case is the test below this one.
    #[test]
    fn messages_with_no_agent_equivalent_are_dropped() {
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 2560, h: 1440 }, NO_CAPS, UNSCALED),
            None
        );
        assert_eq!(
            to_agent(
                &ClientMsg::Connect {
                    force: false,
                    target: "mac".to_owned()
                },
                NO_CAPS,
                UNSCALED
            ),
            None
        );
        assert_eq!(to_agent(&ClientMsg::Disconnect, NO_CAPS, UNSCALED), None);
    }

    // A viewport is remote *pixels* and a display mode is *points*, so the one
    // thing this translation must get right is the division — by the scale this
    // engine last announced, which is the number the client multiplied its window
    // by. Getting it wrong is invisible on a 1x target and doubles or halves the
    // Mac's desktop on a Retina one.
    #[test]
    fn a_viewport_becomes_a_resize_in_display_points() {
        // A 1440x900-point window on a 2x display: the client sent the pixels its
        // canvas is laid out in, and the Mac must be asked for the points.
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 2880, h: 1800 }, RESIZE, 2.0),
            Some(GatewayMsg::ResizeDisplay { w: 1440, h: 900 })
        );
        // At 1x the two units coincide and nothing is scaled away.
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 1280, h: 800 }, RESIZE, UNSCALED),
            Some(GatewayMsg::ResizeDisplay { w: 1280, h: 800 })
        );
        // A fractional ratio rounds rather than truncating, so a window is never
        // asked for one point less than it has.
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 1281, h: 801 }, RESIZE, 1.5),
            Some(GatewayMsg::ResizeDisplay { w: 854, h: 534 })
        );
        // A scale that never happened. `scale_ratio` already refuses nonsense, so
        // reaching here means something upstream broke — and dividing by it would
        // be an infinity, where assuming the units agree is merely wrong.
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 1280, h: 800 }, RESIZE, 0.0),
            Some(GatewayMsg::ResizeDisplay { w: 1280, h: 800 })
        );
    }

    // The sizeless request beside it, and the point of the test is what it does
    // *not* do: no division, whatever the scale, because there is no number on it
    // to divide. The `scale` argument is varied to prove it is ignored.
    #[test]
    fn a_default_size_request_passes_through_unconverted() {
        for scale in [UNSCALED, 2.0, 1.5, 0.0] {
            assert_eq!(
                to_agent(&ClientMsg::DefaultSize, RESIZE, scale),
                Some(GatewayMsg::DefaultDisplaySize)
            );
        }
    }

    // The two flags gate different messages, and this is the test that catches
    // them being swapped at a call site — which, while they were two bare bools
    // sitting next to each other, compiled.
    #[test]
    fn the_two_capability_flags_gate_different_messages() {
        assert_eq!(to_agent(&ClientMsg::ClipboardRequest, RESIZE, UNSCALED), None);
        assert_eq!(
            to_agent(&ClientMsg::Viewport { w: 1280, h: 800 }, CLIPBOARD, UNSCALED),
            None
        );
        // Both size requests answer to the operator's `resize`, not to the
        // clipboard flag — the sizeless one is not a way around the gate.
        assert_eq!(to_agent(&ClientMsg::DefaultSize, CLIPBOARD, UNSCALED), None);
        assert_eq!(to_agent(&ClientMsg::DefaultSize, NO_CAPS, UNSCALED), None);
    }

    // The clipboard pair is the only thing the flag gates, and it gates both
    // directions: a target that didn't opt in neither reads nor writes the
    // Mac's pasteboard, whatever the browser sends.
    #[test]
    fn clipboard_messages_reach_the_agent_only_when_the_target_opted_in() {
        assert_eq!(
            to_agent(&ClientMsg::ClipboardRequest, CLIPBOARD, UNSCALED),
            Some(GatewayMsg::ClipboardRequest)
        );
        assert_eq!(
            to_agent(
                &ClientMsg::Clipboard {
                    text: "copied — 画面".to_owned()
                },
                CLIPBOARD,
                UNSCALED
            ),
            Some(GatewayMsg::Clipboard {
                text: "copied — 画面".to_owned()
            })
        );

        assert_eq!(
            to_agent(&ClientMsg::ClipboardRequest, NO_CAPS, UNSCALED),
            None
        );
        assert_eq!(
            to_agent(
                &ClientMsg::Clipboard {
                    text: "copied".to_owned()
                },
                NO_CAPS,
                UNSCALED
            ),
            None
        );
    }

    // An oversized paste is dropped at the gateway rather than handed to the
    // agent truncated: the Mac keeps the pasteboard it had, instead of gaining a
    // partial copy that looks like the whole thing.
    #[test]
    fn oversized_clipboard_text_never_reaches_the_agent() {
        // Two bytes per char, so this is twice the ceiling.
        let text = "é".repeat(crate::protocol::MAX_CLIPBOARD_BYTES);
        assert_eq!(to_agent(&ClientMsg::Clipboard { text }, CLIPBOARD, UNSCALED), None);

        // At the ceiling it goes through untouched, so the boundary is
        // inclusive and nothing is rewritten on the way past.
        let text = "a".repeat(crate::protocol::MAX_CLIPBOARD_BYTES);
        match to_agent(
            &ClientMsg::Clipboard {
                text: text.clone(),
            },
            CLIPBOARD,
            UNSCALED,
        ) {
            Some(GatewayMsg::Clipboard { text: sent }) => assert_eq!(sent, text),
            other => panic!("expected a clipboard message, got {other:?}"),
        }
    }

    #[test]
    fn backoff_climbs_to_the_cap_and_stops_there() {
        let mut backoff = BACKOFF_MIN;
        let mut steps = 0;
        while backoff < BACKOFF_MAX {
            backoff = (backoff * 2).min(BACKOFF_MAX);
            steps += 1;
            assert!(steps < 10, "backoff should reach the cap quickly");
        }
        assert_eq!(backoff, BACKOFF_MAX);
        // The cap is a fixed point, so the retry loop never runs away.
        assert_eq!((backoff * 2).min(BACKOFF_MAX), BACKOFF_MAX);
    }

    #[tokio::test]
    async fn idle_discards_input_during_an_outage_and_still_returns() {
        tokio::time::pause();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (frame_tx, _frame_rx) = mpsc::channel(4);
        // A browser dragging the mouse through the whole outage.
        for x in 0..100 {
            input_tx.send(ClientMsg::MouseMove { x, y: x }).unwrap();
        }
        assert!(idle(BACKOFF_MIN, &mut input_rx, &frame_tx).await);
        // Every stale event was drained rather than left to replay.
        assert!(input_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn idle_gives_up_when_the_session_layer_drops_the_engine() {
        tokio::time::pause();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<ClientMsg>();
        let (frame_tx, _frame_rx) = mpsc::channel(4);
        drop(input_tx);
        assert!(!idle(BACKOFF_MIN, &mut input_rx, &frame_tx).await);
    }

    #[tokio::test]
    async fn idle_gives_up_when_the_browser_link_is_gone() {
        tokio::time::pause();
        let (_input_tx, mut input_rx) = mpsc::unbounded_channel::<ClientMsg>();
        let (frame_tx, frame_rx) = mpsc::channel::<ServerMsg>(4);
        drop(frame_rx);
        assert!(!idle(BACKOFF_MIN, &mut input_rx, &frame_tx).await);
    }

    // The whole point: a coarse dirty rect that re-encodes to the bytes already on
    // screen costs nothing, while an actual change still gets through.
    #[test]
    fn the_relay_memo_drops_a_repeat_and_passes_a_change() {
        let mut memo = RelayMemo::default();
        let rect = (0, 64, 3200, 64);
        assert!(memo.is_new(rect, b"first"), "never seen before");
        assert!(!memo.is_new(rect, b"first"), "identical bytes, same rect");
        assert!(memo.is_new(rect, b"second"), "the pixels actually changed");
        assert!(!memo.is_new(rect, b"second"), "and now that one is the repeat");
        assert_eq!(memo.dropped, 2);
    }

    // Keyed by rectangle, because that is all the gateway knows: the same content
    // arriving somewhere else is a paint the client has not been given.
    #[test]
    fn the_relay_memo_does_not_confuse_two_rectangles() {
        let mut memo = RelayMemo::default();
        assert!(memo.is_new((0, 0, 128, 64), b"same"));
        assert!(memo.is_new((0, 64, 128, 64), b"same"), "different y");
        assert!(memo.is_new((0, 0, 64, 64), b"same"), "different w");
        assert_eq!(memo.dropped, 0);
    }

    // A repaint exists to fill a canvas that has nothing on it, so nothing may be
    // withheld from one. Everything that empties a client's canvas — attach,
    // reattach, takeover, resize, a new agent link — goes through `forget`.
    #[test]
    fn forgetting_makes_every_rectangle_paintable_again() {
        let mut memo = RelayMemo::default();
        let rect = (0, 0, 128, 64);
        assert!(memo.is_new(rect, b"pixels"));
        assert!(!memo.is_new(rect, b"pixels"));
        memo.forget();
        assert!(
            memo.is_new(rect, b"pixels"),
            "a repaint must reach a blank canvas even with unchanged pixels"
        );
        assert_eq!(memo.dropped, 0, "the counters reset with the memo");
    }
}
