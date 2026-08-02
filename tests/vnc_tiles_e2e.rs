//! End-to-end test of the VNC engine against a real VNC server.
//!
//! Starts the dummy TigerVNC container (`tests/vnc-dummy/`, VncAuth-protected)
//! with podman or docker, points the real axum server at it, and connects a
//! raw WebSocket client. Browser automation deliberately does not validate
//! canvas paint timing or pixels (see CLAUDE.md). Xtigervnc
//! serves its root window with no session behind it, so the first
//! non-incremental update request drives real pixels through the whole
//! pipeline: RFB handshake + DES auth -> `ServerMsg::Tile` -> the same
//! binary WS frames the RDP engine emits (tests/rdp_tiles_e2e.rs).
//!
//! This is also the ZRLE test of record — but only because the container paints a
//! pattern on its root window (`tests/vnc-dummy/Containerfile`). TigerVNC sends a
//! solid rectangle as RRE whatever the client preferred, so against a blank desktop
//! this test never reaches the ZRLE encoder at all. With the pattern it does, and a
//! decoder bug fails here rather than going unnoticed. `remotex::vnc_encodings` logs
//! which encoding a server actually chose, which is the way to check.
//!
//! None of the assertions below depend on the encoding: the first update is
//! non-incremental against an empty shadow, so nothing is suppressed and the whole
//! framebuffer arrives however it was encoded.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, Protocol, Security, TargetConfig};
use remotex::server;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

/// `Tile::FORMAT_PNG`, spelled out rather than imported: this test is a
/// stand-in for a client, and a client only has the number.
const TILE_FORMAT_PNG: u8 = 1;

const DESKTOP_W: u32 = 1024;
const DESKTOP_H: u32 = 768;

/// The target's configured size, which is what a `defaultSize` request resolves
/// to. Deliberately none of the other geometries this test sees — not the server's
/// own 1024x768 and not the 800x600 the viewport asks for — so an assertion on it
/// cannot pass by accident.
const DEFAULT_W: u32 = 1280;
const DEFAULT_H: u32 = 800;

/// Wait until the VNC server actually answers RFB on the published port.
///
/// A bare TCP-accept probe is not enough: rootless podman's port forwarder
/// accepts immediately and then resets if nothing listens inside yet. An RFB
/// server leads with its 12-byte version greeting, so require that.
async fn wait_for_vnc_port(port: u16) {
    use tokio::io::AsyncReadExt as _;

    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let attempt = async {
                let mut stream = TcpStream::connect((common::container_host(), port)).await.ok()?;
                let mut greeting = [0u8; 12];
                stream.read_exact(&mut greeting).await.ok()?;
                greeting.starts_with(b"RFB ").then_some(())
            };
            match tokio::time::timeout(Duration::from_secs(2), attempt).await {
                Ok(Some(())) => return,
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    })
    .await
    .expect("dummy VNC server never sent an RFB greeting");
}

/// Start the real server pointed at the dummy VNC target.
async fn spawn_app(vnc_port: u16) -> SocketAddr {
    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: Some("frontend/dist".into()),
        auth: common::test_auth(),
        branding: "remotex".to_owned(),
        dev_hostname: None,
        targets: vec![TargetConfig {
            name: "tigervnc-dummy".to_owned(),
            protocol: Protocol::Vnc,
            subtype: None,
            host: common::container_host(),
            port: vnc_port,
            username: String::new(),
            // Must match tests/vnc-dummy/Containerfile — exercises VncAuth.
            password: String::new(),
            vnc_password: "secret42".to_owned(),
            domain: None,
            // Not the connect-time size for VNC — the server keeps its own — but
            // the size a `defaultSize` request resolves to.
            width: DEFAULT_W as u16,
            height: DEFAULT_H as u16,
            security: Security::Auto, // RDP-only knob, ignored for VNC
            resize: true,             // exercise the dynamic resize path
            clipboard: true,          // exercise the clipboard bridge
            audio: false,             // VNC has no audio channel at all
            render_type: remotex::config::RenderType::Full,
            render_subtype: remotex::config::RenderSubtype::Png,
            render_quality: None,
            render_motion_subtype: None,
            render_motion_quality: None,
        }],
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Validate one binary batch frame against the desktop bounds and return the
/// pixel area of every tile in it.
fn check_tile_frame(
    stream: &mut common::TileStream,
    frame: &[u8],
    desktop_w: u32,
    desktop_h: u32,
) -> u64 {
    let tiles = stream.paint(frame);
    assert!(!tiles.is_empty(), "a batch frame with no tiles in it");
    let mut area = 0u64;
    for tile in tiles {
        assert_eq!(tile.format, TILE_FORMAT_PNG, "unexpected tile format byte");
        let (x, y, w, h) = (tile.x, tile.y, tile.w, tile.h);
        assert!(w > 0 && h > 0, "empty tile {w}x{h}");
        assert!(
            u32::from(x) + u32::from(w) <= desktop_w && u32::from(y) + u32::from(h) <= desktop_h,
            "tile {w}x{h}+{x}+{y} exceeds the {desktop_w}x{desktop_h} desktop"
        );
        // Length first: a malformed payload is exactly what these markers are here
        // to catch, and slicing a short one would panic on the index instead of
        // reporting what was wrong.
        assert!(
            tile.payload.len() >= 8,
            "payload is {} bytes, too short to be a PNG",
            tile.payload.len()
        );
        assert_eq!(
            &tile.payload[..8],
            b"\x89PNG\r\n\x1a\n",
            "payload is not a PNG stream"
        );
        area += u64::from(w) * u64::from(h);
    }
    area
}

/// Validate a `cursor` control message. Either shape is legitimate: a
/// decodable PNG with its hotspot inside it, or a null image — the server
/// saying it has hidden the pointer, which is what Xtigervnc reports for a
/// root window with no cursor set. Both mean the same thing to the browser:
/// the server is not drawing the pointer, so the browser must.
fn check_cursor_msg(text: &str) {
    use base64::Engine as _;

    let msg: serde_json::Value = serde_json::from_str(text).expect("cursor message is JSON");
    let (w, h) = (msg["w"].as_u64().unwrap(), msg["h"].as_u64().unwrap());
    // `get`, not indexing: a missing field must not pass as an explicit null —
    // the browser distinguishes "no image" from a field the server forgot.
    let image = msg.get("image").expect("cursor message carries an image field");
    let Some(image) = image.as_str() else {
        assert!(image.is_null(), "image must be a string or null: {text}");
        assert_eq!((w, h), (0, 0), "a hidden pointer has no dimensions: {text}");
        return;
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image)
        .expect("image is base64");
    // Decode it fully: a truncated or mislabelled stream must fail here, and
    // the PNG's own dimensions must agree with the ones on the message.
    let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
        .read_info()
        .expect("image is a PNG");
    let mut buf = vec![0; reader.output_buffer_size().expect("PNG size fits in memory")];
    let info = reader.next_frame(&mut buf).expect("PNG decodes");
    assert_eq!(info.color_type, png::ColorType::Rgba, "cursors carry alpha: {text}");
    assert_eq!(
        (u64::from(info.width), u64::from(info.height)),
        (w, h),
        "PNG dimensions disagree with the message: {text}"
    );
    assert!(w > 0 && h > 0, "empty cursor {w}x{h}");
    assert!(
        msg["hx"].as_u64().unwrap() < w && msg["hy"].as_u64().unwrap() < h,
        "hotspot outside the {w}x{h} cursor: {text}"
    );
}

#[tokio::test]
#[ignore = "requires Docker or Podman"]
async fn vnc_session_paints_the_full_desktop_as_tiles_and_resizes() {
    common::init_logging();
    let runtime = common::container_runtime();
    let (_container, vnc_port) =
        common::start_dummy_server(runtime, "remotex-e2e-tigervnc", "vnc-dummy", 5900);
    wait_for_vnc_port(vnc_port).await;

    let addr = spawn_app(vnc_port).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    // The fresh attach lands on the picker; pick the target to start the engine.
    common::connect_target(&mut ws, "tigervnc-dummy").await;

    let mut got_resize = false;
    let mut covered: u64 = 0;
    let mut cursor: Option<String> = None;
    // Resolves cache references, so `covered` counts pixels *painted* rather than
    // pixels that happened to travel. One per socket, because the gateway's slot
    // table lives exactly as long as one attachment.
    let mut stream = common::TileStream::new();

    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    // The only text frames are control messages; the session
                    // must not fail, and resize must precede any tile.
                    assert!(
                        !text.contains(r#""type":"error""#),
                        "session failed: {text}"
                    );
                    if text.contains(r#""type":"resize""#) {
                        assert_eq!(covered, 0, "resize arrived after tiles");
                        // The size announced must be the VNC server's actual
                        // desktop, not the (RDP-oriented) configured 1280x800.
                        assert_eq!(
                            text,
                            format!(r#"{{"type":"resize","w":{DESKTOP_W},"h":{DESKTOP_H},"scale":1.0}}"#)
                        );
                        got_resize = true;
                    }
                    if text.contains(r#""type":"cursor""#) {
                        check_cursor_msg(&text);
                        cursor = Some(text.to_string());
                    }
                }
                Message::Binary(frame) => {
                    assert!(got_resize, "tile arrived before resize");
                    covered += check_tile_frame(&mut stream, &frame, DESKTOP_W, DESKTOP_H);
                    // The first (non-incremental) update must repaint the whole
                    // desktop; once that much area has arrived, the raw->tile
                    // path is proven. The Cursor pseudo-encoding rides the same
                    // update, so wait for the pointer shape too.
                    if covered >= u64::from(DESKTOP_W) * u64::from(DESKTOP_H)
                        && cursor.is_some()
                    {
                        return;
                    }
                }
                _ => {}
            }
        }
        panic!("websocket closed after {covered} px of tiles without a full paint");
    })
    .await
    .expect("timed out waiting for the full-desktop paint and pointer shape");
    let cursor =
        cursor.expect("Xtigervnc must report its pointer once the Cursor encoding is advertised");

    // Dynamic resize: report a smaller browser viewport. Xtigervnc
    // accepts SetDesktopSize, so the engine must announce the new geometry to
    // the browser and follow with a full repaint at that size.
    const VIEWPORT_W: u32 = 800;
    const VIEWPORT_H: u32 = 600;
    ws.send(Message::Text(
        format!(r#"{{"type":"viewport","w":{VIEWPORT_W},"h":{VIEWPORT_H}}}"#).into(),
    ))
    .await
    .unwrap();

    let mut resized = false;
    let mut covered: u64 = 0;
    // Same socket, so the same stream: a resize does not empty the slot table, and
    // a repaint at the new size is exactly when references start paying off.
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(
                        !text.contains(r#""type":"error""#),
                        "session failed: {text}"
                    );
                    if text.contains(r#""type":"resize""#) {
                        assert_eq!(
                            text,
                            format!(r#"{{"type":"resize","w":{VIEWPORT_W},"h":{VIEWPORT_H},"scale":1.0}}"#)
                        );
                        resized = true;
                    }
                }
                Message::Binary(frame) => {
                    // Updates for the old geometry may still be in flight
                    // until the resize announcement; everything after it must
                    // fit the new desktop.
                    if !resized {
                        continue;
                    }
                    covered += check_tile_frame(&mut stream, &frame, VIEWPORT_W, VIEWPORT_H);
                    if covered >= u64::from(VIEWPORT_W) * u64::from(VIEWPORT_H) {
                        return;
                    }
                }
                _ => {}
            }
        }
        panic!("websocket closed after {covered} px of resized tiles");
    })
    .await
    .expect("timed out waiting for the resize + repaint");

    // The sizeless request, which is what a client with no desktop-shaped window
    // of its own sends: the engine supplies the target's configured size rather
    // than the client naming one. Asserted here because this is the only place the
    // resolution can be checked against a real server — a unit test can only prove
    // which number was passed, not that the server accepted it and the browser was
    // told the truth about what it landed on.
    ws.send(Message::Text(r#"{"type":"defaultSize"}"#.into()))
        .await
        .unwrap();

    let mut restored = false;
    let mut covered: u64 = 0;
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(
                        !text.contains(r#""type":"error""#),
                        "session failed: {text}"
                    );
                    if text.contains(r#""type":"resize""#) {
                        assert_eq!(
                            text,
                            format!(r#"{{"type":"resize","w":{DEFAULT_W},"h":{DEFAULT_H},"scale":1.0}}"#),
                            "defaultSize must resolve to the target's configured size"
                        );
                        restored = true;
                    }
                }
                Message::Binary(frame) => {
                    if !restored {
                        continue;
                    }
                    covered += check_tile_frame(&mut stream, &frame, DEFAULT_W, DEFAULT_H);
                    if covered >= u64::from(DEFAULT_W) * u64::from(DEFAULT_H) {
                        return;
                    }
                }
                _ => {}
            }
        }
        panic!("websocket closed after {covered} px of restored tiles");
    })
    .await
    .expect("timed out waiting for the defaultSize resize + repaint");

    // Detach/reattach: drop the browser, reclaim the slot with the
    // same token, and reattach. The still-running engine must re-announce the
    // geometry the session is *now* at — the configured size the request above
    // restored, not the viewport before it — and repaint the full desktop through
    // a real server.
    ws.close(None).await.unwrap();
    drop(ws);

    let (status, body) =
        common::post_session(addr, &cookie, &format!(r#"{{"sessionId":"{token}"}}"#)).await;
    assert_eq!(status, 200, "reclaim after detach failed: {body}");
    let token: String = serde_json::from_str::<serde_json::Value>(&body).unwrap()["sessionId"]
        .as_str()
        .expect("claim response carries a sessionId")
        .to_owned();
    let mut ws = common::connect_ws(addr, &token, &cookie).await;

    let mut reannounced = false;
    let mut replayed_cursor = false;
    let mut covered: u64 = 0;
    // A new socket is a new attachment, so the gateway starts from an empty table.
    let mut stream = common::TileStream::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(
                        !text.contains(r#""type":"error""#),
                        "session failed: {text}"
                    );
                    if text.contains(r#""type":"resize""#) {
                        assert_eq!(
                            text,
                            format!(r#"{{"type":"resize","w":{DEFAULT_W},"h":{DEFAULT_H},"scale":1.0}}"#),
                            "reattach must announce the session's current size"
                        );
                        reannounced = true;
                    }
                    if text.contains(r#""type":"cursor""#) {
                        // The server only sends the shape when it changes, so
                        // the engine has to replay its cached one — otherwise
                        // the reattached browser draws no pointer at all.
                        assert_eq!(text, cursor, "reattach must replay the pointer shape");
                        replayed_cursor = true;
                    }
                }
                Message::Binary(frame) => {
                    // An update already in flight when the slot was reattached
                    // may land before the Refresh-triggered resize; only count
                    // repaint tiles from the announcement on.
                    if !reannounced {
                        continue;
                    }
                    covered += check_tile_frame(&mut stream, &frame, DEFAULT_W, DEFAULT_H);
                    if covered >= u64::from(DEFAULT_W) * u64::from(DEFAULT_H) && replayed_cursor {
                        return;
                    }
                }
                _ => {}
            }
        }
        panic!("websocket closed after {covered} px of reattach tiles");
    })
    .await
    .expect("timed out waiting for the reattach repaint and pointer replay");

    // Clipboard, against a real RFB implementation. The point here is the wire
    // layout: a malformed ClientCutText desyncs the stream, and the very next
    // thing the engine reads would be misparsed — surfacing as an `error` or a
    // closed session rather than the clipboard reply asserted below.
    //
    // What comes *back* isn't asserted: Xtigervnc serves a root window with no
    // X client to own the selection, so whether it echoes a ServerCutText is
    // its business. The in-process test (tests/protocol_e2e.rs) pins the
    // contents round trip against a scripted server.
    ws.send(Message::text(r#"{"type":"clipboard","text":"remotex e2e"}"#))
        .await
        .unwrap();
    ws.send(Message::text(r#"{"type":"clipboardRequest"}"#)).await.unwrap();

    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(
                        !text.contains(r#""type":"error""#),
                        "the clipboard exchange broke the session: {text}"
                    );
                    if text.contains(r#""type":"clipboard""#) {
                        return;
                    }
                }
                Message::Close(frame) => {
                    panic!("the clipboard exchange closed the session: {frame:?}")
                }
                _ => {}
            }
        }
        panic!("websocket closed while waiting for the clipboard reply");
    })
    .await
    .expect("timed out waiting for the clipboard reply");
}
