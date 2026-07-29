//! End-to-end tests for the `rxa` engine against an in-process fake agent.
//!
//! Container-free by necessity: the real agent is a macOS binary that needs a
//! window server and two TCC grants, so it cannot run in CI at all. What *can*
//! be tested without a Mac is everything between the agent's socket and the
//! browser's WebSocket — and that is the entire gateway half.
//!
//! The fake agent speaks the real protocol: it completes a genuine
//! `Noise_KK_25519_ChaChaPoly_BLAKE2s` handshake against a gateway it has been
//! paired with, sends `Hello`, then a pre-encoded WebP tile and a cursor shape,
//! and it records the input it is sent. Four things are under test:
//!
//! 1. **Pass-through.** A WebP the agent encoded reaches the browser as a
//!    `format = 2` tile frame, byte for byte, and the cursor shape arrives on
//!    the existing `cursor` control channel.
//! 2. **Silent reconnect.** When the agent's connection drops mid-session the
//!    engine reconnects and repaints instead of reporting an error and bouncing
//!    the browser back to the picker. That behaviour is the reason this whole
//!    subsystem exists (see docs/mac-agent-architecture.md), so it gets a test.
//! 3. **Input.** The browser's JSON input arrives at the agent in order and
//!    untranslated, and a `viewport` report is swallowed — the half of a session
//!    that leaves no evidence on screen when it goes wrong.
//! 4. **Session expiry.** Closing the browser releases the agent connection
//!    after the shared reattach grace period.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use common::{Ws, connect_ws};
use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, Protocol, Security, TargetConfig};
use remotex::protocol::Tile;
use remotex::server;
use remotex::session::REATTACH_GRACE_PERIOD;
use rxa_proto::msg::{AgentMsg, CursorImage, DisplayEntry, GatewayMsg, SCALE_ONE, format};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// The fake agent's screen size, in captured pixels.
const AGENT_W: u16 = 320;
const AGENT_H: u16 = 240;

/// This agent shares a Retina display, so its pixels are twice its desktop's
/// points — the case a client has to hear about to present the desktop at its own
/// size rather than at twice it.
const AGENT_SCALE: u16 = 2 * SCALE_ONE;

/// The size a mode change on the Mac lands on, in captured pixels. Nothing on
/// this wire asks for it — see [`a_mode_change_on_the_mac_reaches_the_browser`].
const AGENT_MODE_CHANGE: (u16, u16) = (240, 180);

/// The fake Mac's two displays: the screen somebody is sitting at, and the extra
/// one the agent made for itself. Ids are arbitrary and deliberately not 0 and 1,
/// so nothing can pass by treating them as indexes.
const MAIN_DISPLAY: u32 = 0x0421;
const VIRTUAL_DISPLAY: u32 = 0x0938;

/// The virtual display's captured size, different from the main one's so a
/// switch is visible as a resize rather than only as a checkmark moving.
const VIRTUAL_W: u16 = 3200;
const VIRTUAL_H: u16 = 2000;

/// Cursor hotspot, chosen asymmetric so a transposition would show up.
const HOTSPOT: (u16, u16) = (4, 7);

/// What the fake agent's pasteboard starts out holding. Multi-byte on purpose:
/// unlike VNC's latin-1 cut text, this path is UTF-8 end to end.
const FAKE_PASTEBOARD: &str = "on the Mac — 画面 ☕";
const FAKE_CLIPBOARD_CHANGED_AT_MS: u64 = 1_721_234_567_890;

/// Stand-in for a WebP the agent encoded. The gateway never decodes a tile
/// payload — that is the point of the pass-through design — so these bytes only
/// have to survive the trip unchanged. They start with a real RIFF/WEBP/VP8L
/// header so a human staring at a hexdump isn't misled.
fn fake_webp() -> Vec<u8> {
    let mut data = vec![
        b'R', b'I', b'F', b'F', 0x08, 0x08, 0, 0, b'W', b'E', b'B', b'P', b'V', b'P', b'8', b'L',
    ];
    data.extend((0u16..2048).map(|i| (i % 251) as u8));
    data
}

/// Stand-in for the agent's RGBA cursor PNG, likewise never decoded here.
fn fake_cursor_png() -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend((0u16..64).map(|i| (i % 253) as u8));
    png
}

/// A fake agent listening on an ephemeral port.
///
/// `hang_up_first` makes the *first* accepted connection vanish right after its
/// first paint — the shape a Wi-Fi drop takes — while later connections behave
/// normally, so a test can watch the engine come back. Returns the port, a
/// counter of accepted connections, and the browser input the agent received in
/// arrival order.
async fn spawn_fake_agent(
    keys: AgentKeys,
    hang_up_first: bool,
) -> (
    u16,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<GatewayMsg>,
) {
    spawn_fake_agent_with_mode_change(keys, hang_up_first, None).await
}

/// As [`spawn_fake_agent`], with the Mac changing its own display mode right
/// after the first paint — the only way a resolution ever changes here.
async fn spawn_fake_agent_with_mode_change(
    keys: AgentKeys,
    hang_up_first: bool,
    mode_change: Option<(u16, u16)>,
) -> (
    u16,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<GatewayMsg>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    let active = Arc::new(AtomicUsize::new(0));
    let active_connections = Arc::clone(&active);
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let nth = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let hang_up = hang_up_first && nth == 1;
            let input_tx = input_tx.clone();
            let active = Arc::clone(&active_connections);
            tokio::spawn(async move {
                active.fetch_add(1, Ordering::SeqCst);
                if let Err(e) = serve_fake_agent(stream, keys, hang_up, mode_change, input_tx).await
                {
                    // Expected on the hang-up path and at test teardown.
                    eprintln!("fake agent connection ended: {e}");
                }
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (port, connections, active, input_rx)
}

async fn serve_fake_agent(
    mut stream: TcpStream,
    keys: AgentKeys,
    hang_up: bool,
    mode_change: Option<(u16, u16)>,
    input_tx: mpsc::UnboundedSender<GatewayMsg>,
) -> anyhow::Result<()> {
    let transport =
        rxa_proto::noise::respond(&mut stream, &keys.private, &keys.gateway_public).await?;
    let (read_half, write_half) = stream.into_split();
    let (mut reader, mut writer) = rxa_proto::frame::split(read_half, write_half, transport);

    // Hello is the agent's first frame, before it has heard anything back.
    writer
        .send(
            &AgentMsg::Hello {
                version: rxa_proto::VERSION,
                agent_version: "fake-agent".to_owned(),
                w: AGENT_W,
                h: AGENT_H,
                scale: AGENT_SCALE,
            }
            .encode(),
        )
        .await?;

    // Beside Hello, like the real agent: a client needs the menu before it has a
    // picture.
    let mut active = MAIN_DISPLAY;
    writer.send(&displays_msg(active).encode()).await?;

    // The gateway must ask for the stream before any pixels flow.
    match GatewayMsg::decode(&reader.recv().await?)? {
        GatewayMsg::Attach => {}
        other => anyhow::bail!("expected Attach as the gateway's first message, got {other:?}"),
    }

    paint(&mut writer).await?;
    // Somebody changed the resolution on the Mac. A real agent notices through
    // its capture stream and announces the new size; nothing asked it to.
    if let Some((w, h)) = mode_change {
        writer
            .send(
                &AgentMsg::DisplaySize {
                    w,
                    h,
                    scale: AGENT_SCALE,
                }
                .encode(),
            )
            .await?;
        paint(&mut writer).await?;
    }
    if hang_up {
        // Drop everything: the engine sees EOF mid-session and must reconnect.
        return Ok(());
    }

    // Otherwise behave like the real agent: answer keepalives, repaint on ask.
    // The pasteboard is a plain String here — the real one is NSPasteboard.
    let mut pasteboard = FAKE_PASTEBOARD.to_owned();
    let mut clipboard_changed_at_ms = Some(FAKE_CLIPBOARD_CHANGED_AT_MS);
    loop {
        match GatewayMsg::decode(&reader.recv().await?)? {
            GatewayMsg::Ping { nonce } => {
                writer.send(&AgentMsg::Pong { nonce }.encode()).await?;
            }
            GatewayMsg::Refresh => paint(&mut writer).await?,
            // The real agent restarts its capture stream on the new display and
            // announces the size before any tile drawn at it. The ordering is
            // the part worth reproducing here; the stream is not.
            GatewayMsg::SelectDisplay { id } => {
                let _ = input_tx.send(GatewayMsg::SelectDisplay { id });
                active = id;
                let (w, h) = if id == VIRTUAL_DISPLAY {
                    (VIRTUAL_W, VIRTUAL_H)
                } else {
                    (AGENT_W, AGENT_H)
                };
                writer
                    .send(
                        &AgentMsg::DisplaySize {
                            w,
                            h,
                            scale: AGENT_SCALE,
                        }
                        .encode(),
                    )
                    .await?;
                writer.send(&displays_msg(active).encode()).await?;
                paint(&mut writer).await?;
            }
            // The real agent baselines its NSPasteboard change counter here and
            // pushes only when the counter later moves. This one pushes once,
            // immediately, standing in for a copy on the Mac — the gateway
            // behaviour under test (forward an unprompted Clipboard to the
            // browser) is the same, and it gives the test a deterministic
            // arrival instead of a race against a real pasteboard.
            GatewayMsg::ClipboardWatch { enabled } => {
                let _ = input_tx.send(GatewayMsg::ClipboardWatch { enabled });
                if enabled {
                    writer
                        .send(
                            &AgentMsg::Clipboard {
                                text: pasteboard.clone(),
                                changed_at_ms: clipboard_changed_at_ms,
                                requested: false,
                                oversized_bytes: None,
                            }
                            .encode(),
                        )
                        .await?;
                }
            }
            // Read on request too, for an explicit Fetch.
            GatewayMsg::ClipboardRequest => {
                writer
                    .send(
                        &AgentMsg::Clipboard {
                            text: pasteboard.clone(),
                            changed_at_ms: clipboard_changed_at_ms,
                            requested: true,
                            oversized_bytes: None,
                        }
                        .encode(),
                    )
                    .await?;
            }
            GatewayMsg::Clipboard { text } => {
                pasteboard = text.clone();
                clipboard_changed_at_ms =
                    clipboard_changed_at_ms.map(|timestamp| timestamp.saturating_add(1));
                let _ = input_tx.send(GatewayMsg::Clipboard { text });
            }
            // The real agent clamps this into the envelope its display was
            // created with and then says nothing — the new size arrives later,
            // through the poll that announces any mode change. Neither the clamp
            // nor the delay is what this file can test; what it can test is that
            // the request arrived in *points* and that the size which comes back
            // reaches the browser. So this one honours the request exactly and
            // answers immediately.
            GatewayMsg::ResizeDisplay { w, h } => {
                let _ = input_tx.send(GatewayMsg::ResizeDisplay { w, h });
                writer
                    .send(
                        &AgentMsg::DisplaySize {
                            w: w.saturating_mul(2),
                            h: h.saturating_mul(2),
                            scale: AGENT_SCALE,
                        }
                        .encode(),
                    )
                    .await?;
                // The whole surface is new after a mode change, so the real agent
                // repaints it — and the size must be announced before the pixels
                // drawn at it, which is the ordering worth reproducing.
                paint(&mut writer).await?;
            }
            // Everything else is browser input on its way to the Mac. The real
            // agent injects it; here it is recorded so a test can assert on what
            // actually crossed the wire.
            input => {
                let _ = input_tx.send(input);
            }
        }
    }
}

/// The fake Mac's display list, with `active` marked as the one being shared.
fn displays_msg(active: u32) -> AgentMsg {
    AgentMsg::Displays {
        active,
        displays: vec![
            DisplayEntry {
                id: MAIN_DISPLAY,
                label: "Display 1".to_owned(),
                // Points, as the real agent reports them: half the captured pixels
                // on a 2x display.
                detail: format!("{}×{} at 2x", AGENT_W / 2, AGENT_H / 2),
                w: AGENT_W,
                h: AGENT_H,
                scale: AGENT_SCALE,
                flags: DisplayEntry::MAIN,
            },
            DisplayEntry {
                id: VIRTUAL_DISPLAY,
                label: "Virtual display".to_owned(),
                detail: format!("{}×{} at 2x", VIRTUAL_W / 2, VIRTUAL_H / 2),
                w: VIRTUAL_W,
                h: VIRTUAL_H,
                scale: AGENT_SCALE,
                flags: DisplayEntry::OWNED,
            },
        ],
    }
}

/// One full paint: a WebP tile plus the current pointer shape. The real agent
/// resends the cached cursor here too, so a browser attaching later has a
/// pointer without waiting for the shape to change.
async fn paint<W>(writer: &mut rxa_proto::frame::FrameWriter<W>) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    writer
        .send(
            &AgentMsg::Tile {
                format: format::WEBP,
                x: 0,
                y: 0,
                w: 64,
                h: 64,
                data: fake_webp(),
            }
            .encode(),
        )
        .await?;
    writer
        .send(
            &AgentMsg::Cursor(Some(CursorImage {
                w: 24,
                h: 24,
                hx: HOTSPOT.0,
                hy: HOTSPOT.1,
                png: fake_cursor_png(),
            }))
            .encode(),
        )
        .await?;
    Ok(())
}

/// What the fake agent authenticates with: its own private key, and the public
/// key of the one gateway it will answer.
#[derive(Clone, Copy)]
struct AgentKeys {
    private: [u8; 32],
    gateway_public: [u8; 32],
}

/// A paired gateway and Mac — both halves of what the two config files would
/// hold, minted together so a test does not have to derive one from the other.
struct Pairing {
    agent: AgentKeys,
    /// For `[rxa].private_key` on the gateway.
    gateway_private_key: String,
    /// For the target's `agent_public_key`.
    agent_public_key: String,
}

fn pair() -> Pairing {
    use rxa_proto::key::{Role, generate_private, parse_private, public_of, public_text_of};

    let gateway_private_key = generate_private(Role::Gateway);
    let agent_private_key = generate_private(Role::Agent);
    Pairing {
        agent: AgentKeys {
            private: parse_private(Role::Agent, &agent_private_key).unwrap(),
            gateway_public: public_of(&parse_private(Role::Gateway, &gateway_private_key).unwrap()),
        },
        agent_public_key: public_text_of(Role::Agent, &agent_private_key).unwrap(),
        gateway_private_key,
    }
}

/// Start the real axum server with a single `rxa` target pointed at `port`.
async fn spawn_app(port: u16, keys: &Pairing) -> SocketAddr {
    spawn_app_with(port, keys, false, false).await
}

/// As [`spawn_app`], with the target's clipboard bridge opted in or out.
async fn spawn_app_with_clipboard(port: u16, keys: &Pairing, clipboard: bool) -> SocketAddr {
    spawn_app_with(port, keys, clipboard, false).await
}

/// As [`spawn_app`], with the target's `resize` opted in or out.
async fn spawn_app_with_resize(port: u16, keys: &Pairing, resize: bool) -> SocketAddr {
    spawn_app_with(port, keys, false, resize).await
}

async fn spawn_app_with(
    port: u16,
    keys: &Pairing,
    clipboard: bool,
    resize: bool,
) -> SocketAddr {
    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: std::path::PathBuf::from("frontend/dist"),
        targets: vec![TargetConfig {
            name: "mac".to_owned(),
            protocol: Protocol::Rxa,
            subtype: None,
            host: "127.0.0.1".to_owned(),
            port,
            username: String::new(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: 1280,
            height: 800,
            security: Security::Auto,
            // The operator's half of the permission to resize the display the
            // agent made. Off for every test but the one about it.
            resize,
            clipboard,
            agent_public_key: keys.agent_public_key.clone(),
            // `ConfigFile::resolve` fans `[rxa].private_key` out to each rxa
            // target; these tests build the resolved config directly.
            gateway_private_key: keys.gateway_private_key.clone(),
        }],
        site_passwd: common::test_site_passwd(),
        branding: "remotex".to_owned(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Log in, claim the slot, attach, and pick the `mac` target.
async fn open_session(addr: SocketAddr) -> Ws {
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "mac").await;
    ws
}

/// One complete paint as the browser sees it.
///
/// `resize` is optional because a silent reconnect to an unchanged desktop
/// deliberately sends none — see the reconnect test.
struct Paint {
    resize: Option<String>,
    /// The display list seen on the way to these pixels, if one arrived. Like
    /// `resize`, it is optional because it is only sent when something changed.
    displays: Option<String>,
    tile: Vec<u8>,
    cursor: String,
}

/// Drain the socket until a tile and a cursor have arrived, keeping any resize
/// seen on the way (it always precedes the pixels it applies to).
///
/// Fails on an `error` control message, on a `picker` after the session went
/// live, or on a close — those are precisely the three ways a dropped agent
/// link must *not* surface.
async fn expect_paint(ws: &mut Ws) -> Paint {
    let mut resize = None;
    let mut displays = None;
    let mut tile = None;
    let mut cursor = None;
    let mut live = false;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let msg = ws
                .next()
                .await
                .expect("websocket ended mid-session")
                .expect("websocket receive");
            match msg {
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                    if text.contains(r#""type":"connected""#) {
                        live = true;
                    } else if text.contains(r#""type":"picker""#) {
                        // Before `connect` lands this is the normal attach
                        // state; afterwards it means the engine gave up.
                        assert!(!live, "the engine ended and bounced back to the picker");
                    } else if text.contains(r#""type":"resize""#) {
                        resize = Some(text.to_string());
                    } else if text.contains(r#""type":"displays""#) {
                        displays = Some(text.to_string());
                    } else if text.contains(r#""type":"cursor""#) {
                        cursor = Some(text.to_string());
                    }
                }
                Message::Binary(frame) => tile = Some(frame.to_vec()),
                Message::Close(frame) => panic!("session closed: {frame:?}"),
                _ => {}
            }
            if let (Some(tile), Some(cursor)) = (&tile, &cursor) {
                return Paint {
                    resize: resize.clone(),
                    displays: displays.clone(),
                    tile: tile.clone(),
                    cursor: cursor.clone(),
                };
            }
        }
    })
    .await
    .expect("timed out waiting for a full paint")
}

fn assert_first_paint(paint: &Paint) {
    assert_eq!(
        paint.resize.as_deref(),
        Some(
            format!(r#"{{"type":"resize","w":{AGENT_W},"h":{AGENT_H},"scale":2.0}}"#).as_str()
        ),
        "the initial connect must announce the desktop size and the density it is drawn at"
    );
    assert_paint_pixels(paint);
}

/// The half of a paint that looks the same however the link came up.
///
/// The fake agent sends the same tile every time, so a repaint on a socket that has
/// already had one arrives as a *reference* — the gateway recognising bytes the
/// browser already holds. Either form satisfies the claim this makes, and the
/// reference form makes it more strongly: it can only be sent for byte-identical
/// content.
fn assert_paint_pixels(paint: &Paint) {
    let expected = fake_webp();
    match &common::batch_records(&paint.tile)[..] {
        [common::BatchRecord::Tile(tile)] => {
            assert_eq!(
                tile.format,
                Tile::FORMAT_WEBP,
                "the agent's WebP must reach the browser as format 3, not be re-encoded"
            );
            assert_eq!((tile.x, tile.y, tile.w, tile.h), (0, 0, 64, 64));
            assert_eq!(
                tile.payload,
                expected.as_slice(),
                "tile payload must pass through byte for byte"
            );
        }
        [common::BatchRecord::Reference { x, y, .. }] => {
            assert_eq!((*x, *y), (0, 0), "the repeat is redrawn where it was");
        }
        other => panic!("expected one tile record, got {}", other.len()),
    }

    // The cursor rides the existing control channel, so the frontend's
    // `paintCursor` — built for VNC — needs no changes.
    let image = base64::engine::general_purpose::STANDARD.encode(fake_cursor_png());
    assert_eq!(
        paint.cursor,
        format!(
            r#"{{"type":"cursor","image":"{image}","w":24,"h":24,"hx":{},"hy":{}}}"#,
            HOTSPOT.0, HOTSPOT.1
        )
    );
}

#[tokio::test]
async fn agent_tiles_and_cursor_reach_the_browser_untouched() {
    let keys = pair();
    let (port, connections, _active, _input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app(port, &keys).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "a healthy session should connect exactly once"
    );
}

#[tokio::test]
async fn closing_the_browser_releases_the_agent_connection() {
    let keys = pair();
    let (port, connections, active, _input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app(port, &keys).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);
    assert_eq!(active.load(Ordering::SeqCst), 1);

    tokio::time::pause();
    ws.send(Message::Close(None)).await.unwrap();
    drop(ws);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(REATTACH_GRACE_PERIOD).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
        if active.load(Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert_eq!(
        active.load(Ordering::SeqCst),
        0,
        "agent connection stayed active after the browser closed"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "browser loss must end the RXA session instead of reconnecting it"
    );
}

/// Everything the fake agent has received so far, without waiting for more.
///
/// Safe to read straight after a paint that the message in question preceded:
/// the agent records into this channel before it writes anything back, so a
/// frame that arrived proves the record was already queued.
fn drain_input(rx: &mut mpsc::UnboundedReceiver<GatewayMsg>) -> Vec<GatewayMsg> {
    let mut seen = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        seen.push(msg);
    }
    seen
}

/// Wait for the next input message the fake agent received.
async fn expect_input(rx: &mut mpsc::UnboundedReceiver<GatewayMsg>) -> GatewayMsg {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("timed out waiting for input to reach the agent")
        .expect("the fake agent's input channel closed")
}

#[derive(Debug, PartialEq, Eq)]
struct ClipboardMessage {
    text: String,
    changed_at_ms: Option<u64>,
    requested: bool,
}

/// Drain the socket until a timestamped `clipboard` control message arrives.
/// Fails on an error or a close, like the paint helper.
async fn expect_clipboard(ws: &mut Ws) -> ClipboardMessage {
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                    if text.contains(r#""type":"clipboard""#) {
                        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                        return ClipboardMessage {
                            text: parsed["text"].as_str().unwrap().to_owned(),
                            changed_at_ms: parsed["changedAtMs"].as_u64(),
                            requested: parsed["requested"].as_bool().unwrap(),
                        };
                    }
                }
                Message::Close(frame) => panic!("session closed: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for clipboard");
    })
    .await
    .expect("timed out waiting for clipboard")
}

/// How long [`assert_quiet`] listens before deciding nothing is coming. Short
/// because it is spent on every pass, and it only has to cover the hop from the
/// engine to the WebSocket writer task — the fence in the caller has already
/// established that the gateway processed the messages under test.
const QUIET_WINDOW: Duration = Duration::from_millis(500);

/// The inverse of the `expect_*` helpers: assert the browser link stays silent.
///
/// Timing out is the pass condition, hence the short window. Tiles and cursor
/// updates are the engine doing its job and are ignored; a `clipboard` frame,
/// an `error`, or a close are not.
async fn assert_quiet(ws: &mut Ws) {
    // The inner loop only ever leaves by panicking, so the timeout firing is
    // the success path and its result is deliberately discarded.
    let _ = tokio::time::timeout(QUIET_WINDOW, async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(
                        !text.contains(r#""type":"clipboard""#),
                        "a clipboard frame reached a browser that never opted in: {text}"
                    );
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                }
                Message::Close(frame) => panic!("session closed: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended");
    })
    .await;
}

// The other half of a session, and the half with nothing to look at afterwards:
// a mistranslated click paints no evidence anywhere, so the browser looks fine
// while the Mac does the wrong thing. This drives the real WebSocket with the
// JSON a browser sends and asserts what reached the agent, in order.
#[tokio::test]
async fn browser_input_reaches_the_agent_in_order_and_untranslated() {
    let keys = pair();
    let (port, _connections, _active, mut input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app(port, &keys).await;

    let mut ws = open_session(addr).await;
    // Wait for the paint, so the session is fully up before any input is sent.
    assert_first_paint(&expect_paint(&mut ws).await);

    // A click-drag, a wheel notch and a shifted keystroke — with a viewport
    // report in the middle, which this engine must swallow (a Mac has no
    // dynamic resize). Sending it *between* input messages is what makes the
    // sequence assertion below prove it was dropped rather than merely delayed.
    for text in [
        r#"{"type":"mouseMove","x":100,"y":50}"#,
        r#"{"type":"mouseButton","button":"left","pressed":true}"#,
        r#"{"type":"viewport","w":2560,"h":1440}"#,
        r#"{"type":"mouseMove","x":101,"y":52}"#,
        r#"{"type":"mouseButton","button":"left","pressed":false}"#,
        r#"{"type":"wheel","dx":0.0,"dy":-100.0}"#,
        r#"{"type":"key","code":"KeyA","pressed":true,"caps":true}"#,
        r#"{"type":"key","code":"KeyA","pressed":false,"caps":true}"#,
    ] {
        ws.send(Message::text(text)).await.unwrap();
    }

    let expected = [
        GatewayMsg::PointerMove { x: 100, y: 50 },
        GatewayMsg::PointerButton {
            button: 0,
            pressed: true,
        },
        GatewayMsg::PointerMove { x: 101, y: 52 },
        GatewayMsg::PointerButton {
            button: 0,
            pressed: false,
        },
        // DOM deltas, sign and units untouched — the agent owns that conversion.
        GatewayMsg::Wheel { dx: 0.0, dy: -100.0 },
        // The DOM code and the browser's authoritative CapsLock flag, verbatim.
        GatewayMsg::Key {
            code: "KeyA".to_owned(),
            pressed: true,
            caps: true,
        },
        GatewayMsg::Key {
            code: "KeyA".to_owned(),
            pressed: false,
            caps: true,
        },
    ];
    for want in expected {
        assert_eq!(expect_input(&mut input).await, want);
    }
}

// The clipboard round trip, which differs from VNC's in two ways worth pinning:
// the fetch is a real request to the Mac rather than a cached push, and the text
// is UTF-8 rather than latin-1.
#[tokio::test]
async fn clipboard_round_trips_through_the_agent_in_utf8() {
    let keys = pair();
    let (port, _connections, _active, mut input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app_with_clipboard(port, &keys, true).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);

    // Attaching turns the agent's pasteboard watcher on, and the push that
    // follows reaches the browser with nothing having asked for it.
    assert_eq!(
        expect_input(&mut input).await,
        GatewayMsg::ClipboardWatch { enabled: true }
    );
    let pushed = expect_clipboard(&mut ws).await;
    assert_eq!(
        pushed,
        ClipboardMessage {
            text: FAKE_PASTEBOARD.to_owned(),
            changed_at_ms: Some(FAKE_CLIPBOARD_CHANGED_AT_MS),
            requested: false,
        }
    );

    // Mac → browser: the agent reads its pasteboard when asked.
    ws.send(Message::text(r#"{"type":"clipboardRequest"}"#)).await.unwrap();
    let fetched = expect_clipboard(&mut ws).await;
    assert_eq!(fetched.text, pushed.text);
    assert_eq!(
        fetched.changed_at_ms, pushed.changed_at_ms,
        "Fetch must preserve the agent-observed clipboard activity timestamp"
    );
    assert!(fetched.requested, "Fetch replies must be marked requested");

    // Browser → Mac. The é/画面/☕ that VNC would have flattened to '?' arrive
    // intact here.
    let sent = "typed in the browser — 画面 ☕";
    ws.send(Message::text(
        serde_json::json!({ "type": "clipboard", "text": sent }).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(
        expect_input(&mut input).await,
        GatewayMsg::Clipboard {
            text: sent.to_owned()
        }
    );

    // And it stuck: fetching again returns what was just written.
    ws.send(Message::text(r#"{"type":"clipboardRequest"}"#)).await.unwrap();
    let first_write = expect_clipboard(&mut ws).await;
    assert_eq!(
        first_write,
        ClipboardMessage {
            text: sent.to_owned(),
            changed_at_ms: Some(FAKE_CLIPBOARD_CHANGED_AT_MS + 1),
            requested: true,
        }
    );

    // A second browser write is distinct clipboard activity even when the
    // fake clock starts from a fixed value.
    let sent_again = "typed in the browser again";
    ws.send(Message::text(
        serde_json::json!({ "type": "clipboard", "text": sent_again }).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(
        expect_input(&mut input).await,
        GatewayMsg::Clipboard {
            text: sent_again.to_owned()
        }
    );
    ws.send(Message::text(r#"{"type":"clipboardRequest"}"#)).await.unwrap();
    let second_write = expect_clipboard(&mut ws).await;
    assert_eq!(
        second_write,
        ClipboardMessage {
            text: sent_again.to_owned(),
            changed_at_ms: Some(FAKE_CLIPBOARD_CHANGED_AT_MS + 2),
            requested: true,
        }
    );
    assert!(
        second_write.changed_at_ms > first_write.changed_at_ms,
        "each browser write must advance the fake clipboard timestamp"
    );
}

// A target that did not opt in gets no clipboard traffic in either direction,
// even though the agent itself would happily answer.
#[tokio::test]
async fn clipboard_is_inert_when_the_rxa_target_did_not_opt_in() {
    let keys = pair();
    let (port, _connections, _active, mut input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app(port, &keys).await; // clipboard defaults to off

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);

    ws.send(Message::text(r#"{"type":"clipboardRequest"}"#)).await.unwrap();
    ws.send(Message::text(r#"{"type":"clipboard","text":"leaked"}"#))
        .await
        .unwrap();
    // A key press behind them is the fence: it can only reach the agent after
    // both clipboard messages were handled, so if it arrives first, they were
    // dropped rather than merely slow. It also proves no ClipboardWatch was
    // sent at attach — that would have been recorded ahead of the key, and the
    // agent would be reading the Mac's pasteboard for a target that said no.
    ws.send(Message::text(
        r#"{"type":"key","code":"KeyA","pressed":true,"caps":false}"#,
    ))
    .await
    .unwrap();
    assert_eq!(
        expect_input(&mut input).await,
        GatewayMsg::Key {
            code: "KeyA".to_owned(),
            pressed: true,
            caps: false,
        }
    );

    // The other direction, checked once the fence above proves the gateway is
    // past both clipboard messages: anything it meant to send back is already
    // on its way. Nothing may be — not a `clipboard` frame (the engine drops an
    // agent push for a target that said no, and the browser writes an incoming
    // one straight into the real OS clipboard) and not an `error`, since a
    // clipboard message the target didn't opt into is ignored, not a failure.
    assert_quiet(&mut ws).await;
}

// Resolution travels one way here: the Mac changes its own mode — in System
// Settings, or wherever else — and the browser is told. Nothing the browser can
// send asks for it, which is what the viewport report in
// `browser_input_reaches_the_agent_in_order_and_untranslated` proves from the
// other side.
#[tokio::test]
async fn a_mode_change_on_the_mac_reaches_the_browser() {
    let keys = pair();
    let (port, _connections, _active, _input) =
        spawn_fake_agent_with_mode_change(keys.agent, false, Some(AGENT_MODE_CHANGE)).await;
    let addr = spawn_app(port, &keys).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);

    // The second paint carries the new size, and the scale with it: a mode
    // switch can change the density, and a client that missed that would draw
    // the desktop at half or twice its size.
    let (w, h) = AGENT_MODE_CHANGE;
    let repaint = expect_paint(&mut ws).await;
    assert_eq!(
        repaint.resize.as_deref(),
        Some(format!(r#"{{"type":"resize","w":{w},"h":{h},"scale":2.0}}"#).as_str())
    );
    assert_paint_pixels(&repaint);
}

// The unit the two ends disagree in, driven through the real socket. A client's
// viewport is remote *pixels* — its window times the density the gateway
// announced — and a display mode is *points*, so the gateway divides on the way
// through. Nothing downstream can catch this being wrong: the agent would happily
// resize to twice what was asked for, the browser would show it, and the only
// symptom is a Mac desktop that is the wrong size.
#[tokio::test]
async fn a_viewport_reaches_the_agent_as_display_points() {
    let keys = pair();
    let (port, _connections, _active, mut input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app_with_resize(port, &keys, true).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);

    // A 640x400-point window, reported by a client drawing a 2x remote.
    ws.send(Message::text(r#"{"type":"viewport","w":1280,"h":800}"#))
        .await
        .unwrap();
    assert_eq!(
        expect_input(&mut input).await,
        GatewayMsg::ResizeDisplay { w: 640, h: 400 },
        "halved by the scale the gateway announced, not passed through"
    );

    // And the size the Mac lands on comes back the way every other mode change
    // does, so the client is told in pixels what it asked for in its own units.
    let repaint = expect_paint(&mut ws).await;
    assert_eq!(
        repaint.resize.as_deref(),
        Some(r#"{"type":"resize","w":1280,"h":800,"scale":2.0}"#)
    );
}

// The sizeless request beside it, which is what a phone sends: no arithmetic on
// the way through, because there is no number on it to convert. Asserted through
// the socket for the same reason as the test above — the unit conversion is the one
// mistake nothing downstream would report, and the way to prove it cannot happen
// here is to watch a `defaultSize` arrive with no size attached.
#[tokio::test]
async fn a_default_size_request_reaches_the_agent_carrying_no_size() {
    let keys = pair();
    let (port, _connections, _active, mut input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app_with_resize(port, &keys, true).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);

    ws.send(Message::text(r#"{"type":"defaultSize"}"#))
        .await
        .unwrap();
    assert_eq!(
        expect_input(&mut input).await,
        GatewayMsg::DefaultDisplaySize,
        "the created size is the agent's to name, so nothing is sent with the request"
    );
}

// The other half of the gate. `resize` is the operator's permission, and without
// it the request stops at the gateway — the clients hide the control too, so this
// is the belt to their braces. Both size requests answer to it: the sizeless one is
// not a way around a target that said no.
#[tokio::test]
async fn a_viewport_stops_at_the_gateway_when_the_target_did_not_opt_in() {
    let keys = pair();
    let (port, _connections, _active, mut input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app_with_resize(port, &keys, false).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);

    // Sent *before* a message that does travel, so the assertion below proves the
    // two size requests were dropped rather than merely slower than the test.
    for text in [
        r#"{"type":"viewport","w":1280,"h":800}"#,
        r#"{"type":"defaultSize"}"#,
        r#"{"type":"mouseMove","x":10,"y":20}"#,
    ] {
        ws.send(Message::text(text)).await.unwrap();
    }
    assert_eq!(
        expect_input(&mut input).await,
        GatewayMsg::PointerMove { x: 10, y: 20 },
        "neither size request should have reached the agent at all"
    );
}

/// The one display list a client is expected to render, with `active` marked.
fn expected_displays(active: u32) -> String {
    format!(
        r#"{{"type":"displays","active":{active},"displays":[\
{{"id":{MAIN_DISPLAY},"label":"Display 1","detail":"{main_w}×{main_h} at 2x","main":true,"virtual":false}},\
{{"id":{VIRTUAL_DISPLAY},"label":"Virtual display","detail":"{virtual_w}×{virtual_h} at 2x","main":false,"virtual":true}}]}}"#,
        main_w = AGENT_W / 2,
        main_h = AGENT_H / 2,
        virtual_w = VIRTUAL_W / 2,
        virtual_h = VIRTUAL_H / 2
    )
    .replace("\\\n", "")
}

// Which display to look at is the browser's to choose, and the only display
// decision it gets: the round trip has to carry the selection to the Mac and
// bring back the new size *before* any pixels drawn at it.
#[tokio::test]
async fn the_browser_can_pick_which_display_the_mac_shares() {
    let keys = pair();
    let (port, _connections, _active, mut input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app(port, &keys).await;

    let mut ws = open_session(addr).await;
    let first = expect_paint(&mut ws).await;
    assert_first_paint(&first);
    assert_eq!(
        first.displays.as_deref(),
        Some(expected_displays(MAIN_DISPLAY).as_str()),
        "the menu has to arrive with the desktop, not after the first change"
    );

    ws.send(Message::Text(
        format!(r#"{{"type":"selectDisplay","id":{VIRTUAL_DISPLAY}}}"#).into(),
    ))
    .await
    .expect("send selectDisplay");

    let switched = expect_paint(&mut ws).await;
    assert_eq!(
        switched.resize.as_deref(),
        Some(format!(r#"{{"type":"resize","w":{VIRTUAL_W},"h":{VIRTUAL_H},"scale":2.0}}"#).as_str()),
        "a different display is a different size, and the canvas must be told first"
    );
    assert_eq!(
        switched.displays.as_deref(),
        Some(expected_displays(VIRTUAL_DISPLAY).as_str()),
        "the checkmark follows what the agent says is active, not what was clicked"
    );
    assert_paint_pixels(&switched);

    let received = drain_input(&mut input);
    assert!(
        received.iter().any(|msg| matches!(
            msg,
            GatewayMsg::SelectDisplay { id } if *id == VIRTUAL_DISPLAY
        )),
        "the selection must reach the Mac itself: {received:?}"
    );
}

// A browser that attaches mid-session missed the list the agent sent beside its
// Hello. It comes from the gateway's cache rather than by asking the agent,
// because a refresh is a repaint and the displays have not changed.
#[tokio::test]
async fn a_refreshing_browser_gets_the_display_list_again() {
    let keys = pair();
    let (port, _connections, _active, mut input) = spawn_fake_agent(keys.agent, false).await;
    let addr = spawn_app(port, &keys).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);

    ws.send(Message::Text(r#"{"type":"refresh"}"#.into()))
        .await
        .expect("send refresh");

    let repaint = expect_paint(&mut ws).await;
    assert_eq!(
        repaint.displays.as_deref(),
        Some(expected_displays(MAIN_DISPLAY).as_str()),
        "a reattaching browser has no display menu until it is told again"
    );
    assert_paint_pixels(&repaint);
    assert!(
        !drain_input(&mut input)
            .iter()
            .any(|msg| matches!(msg, GatewayMsg::SelectDisplay { .. })),
        "re-announcing must not look like a selection to the agent"
    );
}

// The whole reason for this subsystem: an established session that drops comes
// back on its own, with no error, no credential prompt, and no trip through the
// picker.
#[tokio::test]
async fn a_dropped_agent_link_reconnects_and_repaints_instead_of_erroring() {
    let keys = pair();
    let (port, connections, _active, _input) = spawn_fake_agent(keys.agent, true).await;
    let addr = spawn_app(port, &keys).await;

    let mut ws = open_session(addr).await;
    assert_first_paint(&expect_paint(&mut ws).await);

    // The agent has now vanished. The engine should dial again on its own and
    // repaint — `expect_paint` fails on an error, a picker, or a close.
    let repaint = expect_paint(&mut ws).await;
    assert_paint_pixels(&repaint);
    assert_eq!(
        repaint.resize, None,
        "the Mac came back the same size, so the reconnect must not resize the \
         browser's canvas (a resize clears it) — got {:?}",
        repaint.resize
    );
    assert!(
        connections.load(Ordering::SeqCst) >= 2,
        "the engine should have reconnected, saw {} connection(s)",
        connections.load(Ordering::SeqCst)
    );
}

// Being pointed at a Mac this gateway is not paired with is a configuration
// mistake, not a transient fault: it must be reported immediately rather than
// disappearing into the retry loop.
#[tokio::test]
async fn a_wrong_agent_public_key_is_reported_instead_of_retried_forever() {
    let keys = pair();
    let (port, connections, _active, _input) = spawn_fake_agent(keys.agent, false).await;
    // Well-formed keys throughout — the target just names some other Mac, which
    // is what a stale `agent_public_key` looks like after one is re-imaged.
    let stranger = pair();
    let addr = spawn_app(
        port,
        &Pairing {
            agent_public_key: stranger.agent_public_key,
            ..keys
        },
    )
    .await;

    let mut ws = open_session(addr).await;
    let text = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match ws.next().await.expect("ws ended").expect("ws receive") {
                Message::Text(text) if text.contains(r#""type":"error""#) => {
                    return text.to_string();
                }
                Message::Binary(_) => panic!("an unpaired agent must not paint anything"),
                _ => {}
            }
        }
    })
    .await
    .expect("an unpaired agent should be reported, not retried silently");
    assert!(text.contains("rxa connect failed"), "{text}");

    // Reported is only half of it: "instead of retried" needs the counter. Wait
    // out more than the engine's minimum reconnect backoff (1s) — a retry loop
    // would have dialled again by now, and the fake agent counts every accept.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "a failed initial handshake must not be retried"
    );
}
