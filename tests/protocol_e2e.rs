//! Protocol-level end-to-end tests.
//!
//! These drive the real axum server (HTTP + the `/ws` WebSocket) but do NOT
//! use a real browser or real remote-desktop servers. Two fakes stand in for
//! the remote end:
//!
//! - an "RDP" endpoint that drops every connection, so the session fails to
//!   connect and the failure is reported back over the WebSocket as a
//!   `ServerMsg::Error` — exercising the full bridge (claim → upgrade → input
//!   parsing → rdp session → serialized `ServerMsg` out);
//! - a scripted RFB 3.8 server (security None, 16x16 raw framebuffer) that
//!   stays alive, so the session-slot semantics — claim conflicts,
//!   forced takeover with eviction, detach/reattach with a full repaint — run
//!   against a live engine deterministically.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{Ws, connect_ws};
use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, Protocol, Security, TargetConfig};
use remotex::protocol::MAX_CLIPBOARD_BYTES;
use remotex::server;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const FAKE_DESKTOP: u16 = 16;

/// A fake "RDP" endpoint that accepts connections and immediately drops them.
///
/// This makes the RDP handshake fail deterministically (the peer resets before
/// negotiation completes) without racing on connection-refused and without
/// colliding with another parallel test's ephemeral port — we own this port for
/// the test's lifetime. Returns the port it listens on.
async fn spawn_rejecting_rdp() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream); // hang up immediately
        }
    });
    port
}

/// A scripted RFB 3.8 server: completes the handshake with security None,
/// announces a 16x16 desktop, then answers every **non-incremental**
/// FramebufferUpdateRequest with one full raw-encoded update (incremental
/// requests are left pending, like a real server with no screen changes).
/// Everything else the engine sends is consumed and ignored.
async fn spawn_fake_vnc() -> u16 {
    spawn_fake_vnc_with_clipboard(None).await.0
}

/// As [`spawn_fake_vnc`], but the server announces `cut_text` as its clipboard
/// and reports every `ClientCutText` it receives on the returned channel.
///
/// `cut_text` is raw bytes, not a `str`, because RFB cut text is latin-1 — a
/// UTF-8 literal here would test the wrong wire format.
///
/// The announcement is written *before* the framebuffer update that answers the
/// same request, which is what makes the test deterministic: RFB is one ordered
/// stream, so a browser that has seen the tile is guaranteed to be talking to an
/// engine that has already filed the clipboard.
async fn spawn_fake_vnc_with_clipboard(
    cut_text: Option<&'static [u8]>,
) -> (u16, mpsc::UnboundedReceiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = serve_fake_vnc(stream, cut_text, tx).await;
            });
        }
    });
    (port, rx)
}

async fn serve_fake_vnc(
    mut stream: TcpStream,
    cut_text: Option<&'static [u8]>,
    received_cut_text: mpsc::UnboundedSender<Vec<u8>>,
) -> std::io::Result<()> {
    // Version + security (None) + ClientInit/ServerInit.
    stream.write_all(b"RFB 003.008\n").await?;
    stream.read_exact(&mut [0u8; 12]).await?; // client version
    stream.write_all(&[1, 1]).await?; // one security type: None
    stream.read_exact(&mut [0u8; 1]).await?; // client's choice
    stream.write_all(&0u32.to_be_bytes()).await?; // SecurityResult: ok
    stream.read_exact(&mut [0u8; 1]).await?; // ClientInit (shared flag)

    let mut server_init = Vec::new();
    server_init.extend_from_slice(&FAKE_DESKTOP.to_be_bytes());
    server_init.extend_from_slice(&FAKE_DESKTOP.to_be_bytes());
    server_init.extend_from_slice(&[0u8; 16]); // native pixel format (overridden)
    server_init.extend_from_slice(&4u32.to_be_bytes());
    server_init.extend_from_slice(b"fake");
    stream.write_all(&server_init).await?;

    loop {
        let mut msg_type = [0u8; 1];
        stream.read_exact(&mut msg_type).await?;
        match msg_type[0] {
            // SetPixelFormat
            0 => {
                stream.read_exact(&mut [0u8; 19]).await?;
            }
            // SetEncodings
            2 => {
                let mut head = [0u8; 3];
                stream.read_exact(&mut head).await?;
                let count = u16::from_be_bytes([head[1], head[2]]);
                let mut encodings = vec![0u8; usize::from(count) * 4];
                stream.read_exact(&mut encodings).await?;
            }
            // FramebufferUpdateRequest
            3 => {
                let mut req = [0u8; 9];
                stream.read_exact(&mut req).await?;
                if req[0] != 0 {
                    continue; // incremental: nothing changed, stay quiet
                }
                // ServerCutText first, so the engine has filed it by the time
                // the tile from the same request reaches the browser.
                if let Some(text) = cut_text {
                    let mut msg = vec![3u8, 0, 0, 0]; // type + 3 padding
                    msg.extend_from_slice(&(text.len() as u32).to_be_bytes());
                    msg.extend_from_slice(text);
                    stream.write_all(&msg).await?;
                }
                let mut update = vec![0u8, 0]; // FramebufferUpdate + padding
                update.extend_from_slice(&1u16.to_be_bytes()); // one rect
                update.extend_from_slice(&0u16.to_be_bytes()); // x
                update.extend_from_slice(&0u16.to_be_bytes()); // y
                update.extend_from_slice(&FAKE_DESKTOP.to_be_bytes());
                update.extend_from_slice(&FAKE_DESKTOP.to_be_bytes());
                update.extend_from_slice(&0i32.to_be_bytes()); // raw encoding
                // BGRX pixels (the format the engine forces).
                update.extend_from_slice(&vec![
                    0x40u8;
                    usize::from(FAKE_DESKTOP) * usize::from(FAKE_DESKTOP) * 4
                ]);
                stream.write_all(&update).await?;
            }
            // KeyEvent
            4 => {
                stream.read_exact(&mut [0u8; 7]).await?;
            }
            // PointerEvent
            5 => {
                stream.read_exact(&mut [0u8; 5]).await?;
            }
            // ClientCutText
            6 => {
                let mut head = [0u8; 7];
                stream.read_exact(&mut head).await?;
                let len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]);
                let mut body = vec![0u8; len as usize];
                stream.read_exact(&mut body).await?;
                // Raw latin-1 bytes: what the engine put on the wire, not a
                // String, so the test can assert the encoding too.
                let _ = received_cut_text.send(body);
            }
            other => panic!("fake vnc server got unexpected message type {other}"),
        }
    }
}

// ── Apple Screen Sharing (RFB 003.889), scripted ────────────────────────────
//
// The `ard-high-performance` subtype's whole wire, played from the server side:
// Apple's version banner, its DH authentication, the `0xC1` ClientInit, the
// cleartext prelude, the rekey that switches on the record layer, and then a
// display layout and a framebuffer update *inside* that record layer.
//
// This is the only automated test that can reach any of it. There is no
// containerisable Apple server — `tests/vnc-dummy` is Xtigervnc and speaks none of
// this — and a real Mac is manual QA.
//
// Written against the specification rather than by calling into `src/vnc.rs`, so
// that a misreading on one side cannot be agreed with by the other. The one
// exception is the record *framing*, where `RecordWriter`/`RecordReader` are reused:
// they are symmetric by construction, so a second copy here would prove nothing
// that `src/vnc_record.rs`'s own byte-level tests do not already pin, and the
// interesting failures in this test are in the handshake and the plumbing above it.

/// The 1024-bit MODP group of RFC 2409, which is the size macOS offers.
const DH_PRIME: &[u8] = &[
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x0f, 0xda, 0xa2, 0x21, 0x68, 0xc2, 0x34,
    0xc4, 0xc6, 0x62, 0x8b, 0x80, 0xdc, 0x1c, 0xd1, 0x29, 0x02, 0x4e, 0x08, 0x8a, 0x67, 0xcc, 0x74,
    0x02, 0x0b, 0xbe, 0xa6, 0x3b, 0x13, 0x9b, 0x22, 0x51, 0x4a, 0x08, 0x79, 0x8e, 0x34, 0x04, 0xdd,
    0xef, 0x95, 0x19, 0xb3, 0xcd, 0x3a, 0x43, 0x1b, 0x30, 0x2b, 0x0a, 0x6d, 0xf2, 0x5f, 0x14, 0x37,
    0x4f, 0xe1, 0x35, 0x6d, 0x6d, 0x51, 0xc2, 0x45, 0xe4, 0x85, 0xb5, 0x76, 0x62, 0x5e, 0x7e, 0xc6,
    0xf4, 0x4c, 0x42, 0xe9, 0xa6, 0x37, 0xed, 0x6b, 0x0b, 0xff, 0x5c, 0xb6, 0xf4, 0x06, 0xb7, 0xed,
    0xee, 0x38, 0x6b, 0xfb, 0x5a, 0x89, 0x9f, 0xa5, 0xae, 0x9f, 0x24, 0x11, 0x7c, 0x4b, 0x1f, 0xe6,
    0x49, 0x28, 0x66, 0x51, 0xec, 0xe6, 0x53, 0x81, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];
/// The account the target below carries and the fake Mac expects back.
const MAC_USER: &str = "andrew";
const MAC_PASSWORD: &str = "s3cr3t-should-not-leak";
/// The two screens the fake Mac reports, so a display list has something in it.
const MAC_MAIN_DISPLAY: u32 = 0x2b00_4501;
const MAC_SECOND_DISPLAY: u32 = 0x2b00_4502;
/// The framebuffer the layout declares: a 1x logical size equal to its backing
/// size, so the test asserts on one number and the density path stays out of it.
const MAC_DESKTOP: u16 = 32;

/// A scripted macOS Screen Sharing server. Returns the port, and a channel
/// reporting each `SetDisplayMessage` the gateway sends — which is how the test
/// sees a display selection reach the wire.
async fn spawn_fake_mac() -> (u16, mpsc::UnboundedReceiver<u32>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = serve_fake_mac(stream, tx).await;
            });
        }
    });
    (port, rx)
}

/// The server half of Apple's DH authentication: offer the group, then recover the
/// shared secret from the client's public value and check the credentials came back
/// as the account this Mac expects.
///
/// Returns the wrap key — `MD5(shared)` — which is the record layer's first key.
async fn fake_mac_authenticate(stream: &mut TcpStream) -> std::io::Result<[u8; 16]> {
    use md5::{Digest as _, Md5};
    use num_bigint::BigUint;

    let modulus = BigUint::from_bytes_be(DH_PRIME);
    // Fixed, so the test is deterministic. A real server would not reuse it.
    let private = BigUint::from(0x1234_5678_9abc_def0u64);
    let public = BigUint::from(2u8).modpow(&private, &modulus);

    let pad = |value: &BigUint| {
        let bytes = value.to_bytes_be();
        let mut out = vec![0u8; DH_PRIME.len() - bytes.len()];
        out.extend_from_slice(&bytes);
        out
    };

    let mut challenge = Vec::new();
    challenge.extend_from_slice(&2u16.to_be_bytes()); // generator
    challenge.extend_from_slice(&(DH_PRIME.len() as u16).to_be_bytes());
    challenge.extend_from_slice(DH_PRIME);
    challenge.extend_from_slice(&pad(&public));
    stream.write_all(&challenge).await?;

    let mut credentials = [0u8; 128];
    stream.read_exact(&mut credentials).await?;
    let mut peer = vec![0u8; DH_PRIME.len()];
    stream.read_exact(&mut peer).await?;

    let shared = pad(&BigUint::from_bytes_be(&peer).modpow(&private, &modulus));
    let key: [u8; 16] = Md5::digest(&shared).into();

    // AES-128-ECB, the same each-block-independently form the client used.
    {
        use aes::Aes128;
        use aes::cipher::{BlockCipherDecrypt as _, KeyInit as _};
        let cipher = Aes128::new(&key.into());
        let mut plain = credentials;
        for block in plain.chunks_exact_mut(16) {
            cipher.decrypt_block(<&mut [u8; 16]>::try_from(block).unwrap().into());
        }
        let field = |at: usize| {
            let bytes = &plain[at..at + 64];
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(64);
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        };
        assert_eq!(field(0), MAC_USER, "the gateway named the wrong account");
        assert_eq!(field(64), MAC_PASSWORD, "the gateway sent the wrong password");
    }
    stream.write_all(&0u32.to_be_bytes()).await?; // SecurityResult: ok
    Ok(key)
}

/// The `AppleDisplayLayout` payload for this Mac's two screens.
///
/// Built here from the field model rather than by calling the gateway's builder —
/// there is none, the gateway only parses this — so the offsets are asserted from
/// both ends.
fn fake_mac_layout() -> Vec<u8> {
    const RECORD: usize = 0x38;
    const HEAD: usize = 0x14;
    let screens = [(MAC_MAIN_DISPLAY, 0x01u32), (MAC_SECOND_DISPLAY, 0x00)];

    let mut p = vec![0u8; HEAD];
    p[..2].copy_from_slice(&((HEAD + screens.len() * RECORD) as u16).to_be_bytes());
    p[2..4].copy_from_slice(&5u16.to_be_bytes()); // version
    // The leading geometry, which is the first screen's two rects.
    for at in [4, 6, 8, 10] {
        p[at..at + 2].copy_from_slice(&MAC_DESKTOP.to_be_bytes());
    }
    for (id, flags) in screens {
        let mut r = vec![0u8; RECORD];
        r[0x10..0x14].copy_from_slice(&id.to_be_bytes());
        r[0x18..0x1a].copy_from_slice(&MAC_DESKTOP.to_be_bytes()); // logical w
        r[0x1a..0x1c].copy_from_slice(&MAC_DESKTOP.to_be_bytes()); // logical h
        r[0x20..0x22].copy_from_slice(&MAC_DESKTOP.to_be_bytes()); // backing w
        r[0x22..0x24].copy_from_slice(&MAC_DESKTOP.to_be_bytes()); // backing h
        r[0x24..0x28].copy_from_slice(&flags.to_be_bytes());
        p.extend_from_slice(&r);
    }
    p
}

/// One raw framebuffer update covering the whole desktop, in a colour derived from
/// `shade` so two of them are never mistaken for one repeat.
fn fake_mac_update(shade: u8) -> Vec<u8> {
    let mut update = vec![0u8, 0];
    update.extend_from_slice(&1u16.to_be_bytes()); // one rect
    update.extend_from_slice(&0u16.to_be_bytes()); // x
    update.extend_from_slice(&0u16.to_be_bytes()); // y
    update.extend_from_slice(&MAC_DESKTOP.to_be_bytes());
    update.extend_from_slice(&MAC_DESKTOP.to_be_bytes());
    update.extend_from_slice(&0i32.to_be_bytes()); // raw
    update.extend_from_slice(&vec![
        shade;
        usize::from(MAC_DESKTOP) * usize::from(MAC_DESKTOP) * 4
    ]);
    update
}

async fn serve_fake_mac(
    mut stream: TcpStream,
    selected: mpsc::UnboundedSender<u32>,
) -> std::io::Result<()> {
    use remotex::vnc_record::{Keys, RecordReader, RecordWriter};
    use tokio::io::AsyncReadExt as _;

    stream.write_all(b"RFB 003.889\n").await?;
    let mut banner = [0u8; 12];
    stream.read_exact(&mut banner).await?;
    assert_eq!(&banner, b"RFB 003.889\n", "the gateway answered the wrong version");

    // The set a macOS 26 host offers, in its wire order.
    stream.write_all(&[4, 30, 33, 36, 35]).await?;
    let mut chosen = [0u8; 1];
    stream.read_exact(&mut chosen).await?;
    assert_eq!(chosen[0], 30, "the gateway picked the wrong security type");

    let wrap_key = fake_mac_authenticate(&mut stream).await?;

    let mut client_init = [0u8; 1];
    stream.read_exact(&mut client_init).await?;
    assert_eq!(client_init[0], 0xc1, "Apple's ClientInit byte is 0xC1");

    let mut server_init = Vec::new();
    server_init.extend_from_slice(&MAC_DESKTOP.to_be_bytes());
    server_init.extend_from_slice(&MAC_DESKTOP.to_be_bytes());
    server_init.extend_from_slice(&[0u8; 16]);
    server_init.extend_from_slice(&3u32.to_be_bytes());
    server_init.extend_from_slice(b"mac");
    stream.write_all(&server_init).await?;

    // The cleartext prelude: SetEncryption twice, and nothing else.
    //
    // A `ViewerInfo` here would be a bug, and this is where it is caught: a real Mac
    // reads more bytes for that message than its own length declares, swallows the
    // SetEncryption behind it, and then waits forever — with no error from either
    // end. That failure is invisible without a Mac, so the asserts below are the
    // stand-in.
    for expected in [1u16, 2] {
        let mut msg = [0u8; 4];
        stream.read_exact(&mut msg).await?;
        assert_eq!(msg[0], 0x12, "the prelude is SetEncryption and nothing else");
        assert_eq!(u16::from_be_bytes([msg[2], msg[3]]), expected);
        // command 1 carries a method list, command 2 two words.
        let rest = if expected == 1 { 8 } else { 4 };
        stream.read_exact(&mut vec![0u8; rest]).await?;
    }

    // The rekey, wrapped under MD5(shared): from here everything is records.
    let keys = Keys {
        key: *b"apple-record-key",
        iv: *b"apple-record-iv!",
    };
    {
        use aes::Aes128;
        use aes::cipher::{BlockCipherEncrypt as _, KeyInit as _};
        let cipher = Aes128::new(&wrap_key.into());
        let wrapped = |mut block: [u8; 16]| {
            cipher.encrypt_block((&mut block).into());
            block
        };
        let mut rekey = vec![0u8, 0];
        rekey.extend_from_slice(&1u16.to_be_bytes()); // one rect
        rekey.extend_from_slice(&[0u8; 8]); // x, y, w, h all zero
        rekey.extend_from_slice(&0x44fi32.to_be_bytes());
        rekey.extend_from_slice(&1u32.to_be_bytes()); // generation
        rekey.extend_from_slice(&wrapped(keys.key));
        rekey.extend_from_slice(&wrapped(keys.iv));
        stream.write_all(&rekey).await?;
    }

    let (read_half, mut write_half) = stream.into_split();
    let mut records = RecordReader::new(read_half, keys);
    let mut writer = RecordWriter::new(keys);
    let mut shade = 0x40u8;
    let mut sent_layout = false;

    loop {
        let mut kind = [0u8; 1];
        records.read_exact(&mut kind).await?;
        match kind[0] {
            // SetPixelFormat
            0 => {
                records.read_exact(&mut [0u8; 19]).await?;
            }
            // SetEncodings
            2 => {
                let mut head = [0u8; 3];
                records.read_exact(&mut head).await?;
                let count = u16::from_be_bytes([head[1], head[2]]);
                records.read_exact(&mut vec![0u8; usize::from(count) * 4]).await?;
            }
            // FramebufferUpdateRequest. A non-incremental one is answered; the
            // first is answered with the display layout first, which is the
            // metadata burst a real Mac opens with.
            3 => {
                let mut req = [0u8; 9];
                records.read_exact(&mut req).await?;
                if req[0] != 0 {
                    continue;
                }
                if !std::mem::replace(&mut sent_layout, true) {
                    let mut rect = vec![0u8, 0];
                    rect.extend_from_slice(&1u16.to_be_bytes());
                    rect.extend_from_slice(&[0u8; 8]);
                    rect.extend_from_slice(&0x451i32.to_be_bytes());
                    rect.extend_from_slice(&fake_mac_layout());
                    write_half.write_all(writer.frame(&rect).unwrap()).await?;
                }
                shade = shade.wrapping_add(0x10);
                write_half
                    .write_all(writer.frame(&fake_mac_update(shade)).unwrap())
                    .await?;
            }
            // KeyEvent
            4 => {
                records.read_exact(&mut [0u8; 7]).await?;
            }
            // PointerEvent
            5 => {
                records.read_exact(&mut [0u8; 5]).await?;
            }
            // AutoFrameBufferUpdate: the arming, which a real Mac answers by
            // streaming. Here the paired non-incremental request drives it.
            0x09 => {
                records.read_exact(&mut [0u8; 15]).await?;
            }
            // SetDisplayMessage: the whole point of the subtype. Reported to the
            // test, then answered with a layout, which is how a real Mac confirms
            // it acted.
            0x0d => {
                let mut body = [0u8; 7];
                records.read_exact(&mut body).await?;
                assert_eq!(body[0], 0, "the aggregate is not implemented");
                let id = u32::from_be_bytes([body[3], body[4], body[5], body[6]]);
                let _ = selected.send(id);
                let mut rect = vec![0u8, 0];
                rect.extend_from_slice(&1u16.to_be_bytes());
                rect.extend_from_slice(&[0u8; 8]);
                rect.extend_from_slice(&0x451i32.to_be_bytes());
                rect.extend_from_slice(&fake_mac_layout());
                write_half.write_all(writer.frame(&rect).unwrap()).await?;
            }
            // SetDisplayConfiguration
            0x1d => {
                let mut head = [0u8; 3];
                records.read_exact(&mut head).await?;
                let size = u16::from_be_bytes([head[1], head[2]]);
                let mut body = vec![0u8; usize::from(size)];
                records.read_exact(&mut body).await?;
                // Named, because the alternative is an index-out-of-bounds panic
                // naming a byte offset instead of the message that was too short.
                assert!(
                    body.len() >= 8 + 0x9c,
                    "a display configuration carried {} body bytes, too few for a descriptor",
                    body.len()
                );
                // The static descriptor: no dynamic-resolution flag and no virtual
                // display, which is what says this session is fixed-size.
                let d = &body[8..]; // past version, display_count, flags
                assert_eq!(&d[0x7a..0x7e], &[0u8; 4], "no dynamic-resolution flag");
                assert_eq!(&d[0x7e..0x82], &[0u8; 4], "not a virtual display");
                assert_ne!(&d[0x9a..0x9c], &[0u8; 2], "mode_count must not be zero");
            }
            other => panic!("fake Mac got unexpected message type {other:#x}"),
        }
    }
}

/// Install the ring crypto provider once (the binary does this in `main`; tests
/// don't run `main`, so a code path that reaches TLS would otherwise panic).
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

/// Start the server on an ephemeral port against the given target. Returns
/// the bound address.
async fn spawn_app(target: TargetConfig) -> SocketAddr {
    ensure_crypto_provider();
    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: Some("frontend/dist".into()),
        targets: vec![target],
        auth: common::test_auth(),
        branding: "remotex".to_owned(),
        dev_hostname: None,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn target(protocol: Protocol, port: u16) -> TargetConfig {
    target_with_clipboard(protocol, port, false)
}

fn target_with_clipboard(protocol: Protocol, port: u16, clipboard: bool) -> TargetConfig {
    TargetConfig {
        name: "test-target".to_owned(),
        protocol,
        subtype: None,
        host: "127.0.0.1".to_owned(),
        port,
        // A VNC target names no user: the fake server below offers security
        // type None, and a username is a request for Apple's DH authentication,
        // which it cannot answer. Both password fields carry the canary either
        // way — neither may reach the browser.
        username: match protocol {
            Protocol::Vnc => String::new(),
            _ => "tester".to_owned(),
        },
        password: "s3cr3t-should-not-leak".to_owned(),
        vnc_password: String::new(),
        domain: None,
        width: 1280,
        height: 800,
        security: Security::Auto,
        resize: false,
        clipboard,
        audio: false,
        render_type: remotex::config::RenderType::Full,
        render_subtype: remotex::config::RenderSubtype::Png,
        render_quality: None,
    }
}

/// A target for the fake Mac: the high-performance subtype, with the account the
/// fake Mac checks the credentials against.
fn mac_target(port: u16) -> TargetConfig {
    TargetConfig {
        subtype: Some(remotex::config::Subtype::ArdHighPerformance),
        username: MAC_USER.to_owned(),
        password: MAC_PASSWORD.to_owned(),
        ..target(Protocol::Vnc, port)
    }
}

/// Start the app against the connection-dropping RDP endpoint.
async fn spawn_app_dead_rdp() -> SocketAddr {
    let dead_rdp_port = spawn_rejecting_rdp().await;
    spawn_app(target(Protocol::Rdp, dead_rdp_port)).await
}

/// Minimal HTTP/1.1 GET (optionally with the login cookie) returning the
/// response body as a string.
async fn http_get(addr: SocketAddr, path: &str, cookie: Option<&str>) -> String {
    let cookie_line = cookie.map(|c| format!("Cookie: {c}\r\n")).unwrap_or_default();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{cookie_line}Connection: close\r\n\r\n"
    );
    let (_status, _head, body) = common::http_request(addr, &req).await;
    body
}

/// Read from the socket until a `resize` control message arrives; fails on an
/// `error` message or a close.
async fn expect_resize(ws: &mut Ws, w: u16, h: u16) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                    if text.contains(r#""type":"resize""#) {
                        assert_eq!(text, format!(r#"{{"type":"resize","w":{w},"h":{h},"scale":1.0}}"#));
                        return;
                    }
                }
                Message::Close(frame) => panic!("closed while waiting for resize: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for resize");
    })
    .await
    .expect("timed out waiting for resize");
}

/// Read from the socket until the `picker` status control message arrives;
/// fails on an `error` message or a close.
async fn expect_picker(ws: &mut Ws) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                    if text.contains(r#""type":"picker""#) {
                        return;
                    }
                }
                Message::Close(frame) => panic!("closed while waiting for picker: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for picker");
    })
    .await
    .expect("timed out waiting for picker");
}

/// Read from the socket until a binary tile frame arrives.
async fn expect_tile(ws: &mut Ws) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Binary(frame) => {
                    // Parsed rather than sniffed: the envelope's own invariants
                    // are checked on the way past. Records, not painted tiles —
                    // this only cares that paint arrived, and a repeat of pixels
                    // the client already has legitimately arrives as a reference.
                    assert!(!common::batch_records(&frame).is_empty());
                    return;
                }
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                }
                Message::Close(frame) => panic!("closed while waiting for a tile: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for a tile");
    })
    .await
    .expect("timed out waiting for a tile");
}

/// Read from the socket until it closes; returns the close code (if any).
async fn expect_close(ws: &mut Ws) -> Option<u16> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Close(frame)) => return frame.map(|f| u16::from(f.code)),
                Ok(_) => {}
                Err(_) => return None, // connection dropped without a close frame
            }
        }
        None
    })
    .await
    .expect("timed out waiting for the close")
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let addr = spawn_app_dead_rdp().await;
    // Health stays public (it's a liveness probe) — no login cookie.
    let body = http_get(addr, "/api/health", None).await;
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn targets_endpoint_lists_targets_but_never_credentials() {
    let addr = spawn_app_dead_rdp().await;
    let cookie = common::login(addr).await;
    let body = http_get(addr, "/api/targets", Some(&cookie)).await;
    assert!(body.contains("test-target"), "targets should list the name: {body}");
    assert!(body.contains("127.0.0.1"), "targets should report the host: {body}");
    // Credentials must never be serialized to the browser.
    assert!(
        !body.contains("s3cr3t-should-not-leak"),
        "targets leaked the password: {body}"
    );
    assert!(!body.contains("tester"), "targets leaked the username: {body}");
    assert!(!body.contains("password"), "targets mentions a password field: {body}");
}

#[tokio::test]
async fn websocket_reports_rdp_connect_failure_as_error_message() {
    let addr = spawn_app_dead_rdp().await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;

    // Pick the target to start the (dead) engine, then send a realistic input
    // event too (proves both control- and input-message parsing).
    common::connect_target(&mut ws, "test-target").await;
    ws.send(Message::text(r#"{"type":"mouseMove","x":10,"y":20}"#))
        .await
        .unwrap();

    // The RDP target is dead, so the server should push back an error ServerMsg.
    let got_error = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            if let Ok(Message::Text(text)) = msg
                && text.contains(r#""type":"error""#)
            {
                return true;
            }
        }
        false
    })
    .await
    .expect("timed out waiting for an error ServerMsg");

    assert!(got_error, "expected an error ServerMsg after a failed RDP connect");
}

#[tokio::test]
async fn websocket_without_a_valid_token_is_closed_with_4000() {
    let addr = spawn_app_dead_rdp().await;
    let cookie = common::login(addr).await;

    // No token at all (authenticated, so the upgrade itself succeeds).
    let mut ws = connect_ws(addr, "", &cookie).await;
    assert_eq!(expect_close(&mut ws).await, Some(4000));

    // A made-up token.
    let mut ws = connect_ws(addr, "not-a-real-token", &cookie).await;
    assert_eq!(expect_close(&mut ws).await, Some(4000));
}

#[tokio::test]
async fn takeover_evicts_the_attached_browser_and_repaints_for_the_new_one() {
    let vnc_port = spawn_fake_vnc().await;
    let addr = spawn_app(target(Protocol::Vnc, vnc_port)).await;
    let cookie = common::login(addr).await;

    // Browser A claims and attaches, lands on the picker, then picks the
    // target; the engine connects to the fake VNC server and paints the desktop.
    let token_a = common::claim_session(addr, &cookie).await;
    let mut ws_a = connect_ws(addr, &token_a, &cookie).await;
    common::connect_target(&mut ws_a, "test-target").await;
    expect_resize(&mut ws_a, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws_a).await;

    // Browser B: a plain claim is refused while A is attached…
    let (status, _) = common::post_session(addr, &cookie, "{}").await;
    assert_eq!(status, 409, "a live attachment must block a plain claim");
    // …and A's own token reclaims without force (the reconnect path).
    let (status, _) =
        common::post_session(addr, &cookie, &format!(r#"{{"sessionId":"{token_a}"}}"#)).await;
    assert_eq!(status, 200, "the holder reclaims with its token");
    // That reclaim evicted A's socket; reattach A to a fresh one.
    assert_eq!(expect_close(&mut ws_a).await, Some(4001));
    let token_a = common::claim_session(addr, &cookie).await; // nothing attached now
    let mut ws_a = connect_ws(addr, &token_a, &cookie).await;
    expect_resize(&mut ws_a, FAKE_DESKTOP, FAKE_DESKTOP).await;

    // B takes over with force: A is evicted with 4001, A's token dies, and B
    // gets the desktop repainted from the same still-running engine session.
    let (status, body) = common::post_session(addr, &cookie, r#"{"force":true}"#).await;
    assert_eq!(status, 200, "force takeover must succeed: {body}");
    let token_b = serde_json::from_str::<serde_json::Value>(&body).unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_eq!(expect_close(&mut ws_a).await, Some(4001));
    let mut ws_stale = connect_ws(addr, &token_a, &cookie).await;
    assert_eq!(expect_close(&mut ws_stale).await, Some(4000));

    let mut ws_b = connect_ws(addr, &token_b, &cookie).await;
    expect_resize(&mut ws_b, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws_b).await;
}

/// Logging out ends the desktop, and the login after it starts from the picker.
///
/// The counterpart to `detach_keeps_the_engine_and_reattach_repaints` below: losing a
/// browser keeps the engine for its reattach grace, and logging out must not, because
/// the credential that opened the session is gone. It used to take the detach path —
/// closing the socket was all the browser did — so the target stayed connected and a
/// login inside the grace period silently resumed the desktop.
///
/// End to end over HTTP on purpose. The unit tests in `src/session.rs` cover
/// `log_out` itself and pass whether or not `logout_handler` ever calls it, so this
/// is the one that fails if the handler stops.
#[tokio::test]
async fn logging_out_ends_the_desktop_and_the_next_login_starts_at_the_picker() {
    let vnc_port = spawn_fake_vnc().await;
    let addr = spawn_app(target(Protocol::Vnc, vnc_port)).await;
    let cookie = common::login(addr).await;

    // A live desktop: claim, attach, pick the target, and see it paint.
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "test-target").await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws).await;

    let request = format!(
        "POST /api/auth/logout HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Cookie: {cookie}\r\nContent-Length: 0\r\n\r\n"
    );
    let (status, _, _) = common::http_request(addr, &request).await;
    assert_eq!(status, 200);

    // The socket that was watching the desktop is let go, rather than left
    // attached to a slot whose claim has been released.
    assert_eq!(expect_close(&mut ws).await, Some(4001));

    // The whole point: log in again and there is no desktop to inherit. Without the
    // teardown this reports `connected` and paints the session that was logged out
    // of, for as long as the reattach grace period lasts.
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    expect_picker(&mut ws).await;
}

#[tokio::test]
async fn detach_keeps_the_engine_and_reattach_repaints() {
    let vnc_port = spawn_fake_vnc().await;
    let addr = spawn_app(target(Protocol::Vnc, vnc_port)).await;
    let cookie = common::login(addr).await;

    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "test-target").await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws).await;

    // Detach: the browser goes away, the engine keeps running.
    ws.close(None).await.unwrap();
    drop(ws);

    // Reattach (same token, reclaim): the engine must re-announce the size
    // and repaint the whole desktop from the running session.
    let (status, body) =
        common::post_session(addr, &cookie, &format!(r#"{{"sessionId":"{token}"}}"#)).await;
    assert_eq!(status, 200, "reclaim after detach failed: {body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut ws = connect_ws(addr, &token, &cookie).await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws).await;
}

#[tokio::test]
async fn attach_lands_on_the_picker_and_takeover_inherits_it() {
    // No target is ever connected, so no engine runs (a dead RDP endpoint is
    // fine — it's never dialed).
    let addr = spawn_app_dead_rdp().await;
    let cookie = common::login(addr).await;

    // Browser A attaches and, having picked nothing, lands on the picker.
    let token_a = common::claim_session(addr, &cookie).await;
    let mut ws_a = connect_ws(addr, &token_a, &cookie).await;
    expect_picker(&mut ws_a).await;

    // Browser B force-claims: A is evicted, and B inherits the picker state
    // (not a desktop), because that is where the slot was.
    let (status, body) = common::post_session(addr, &cookie, r#"{"force":true}"#).await;
    assert_eq!(status, 200, "force takeover must succeed: {body}");
    let token_b = serde_json::from_str::<serde_json::Value>(&body).unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(expect_close(&mut ws_a).await, Some(4001));

    let mut ws_b = connect_ws(addr, &token_b, &cookie).await;
    expect_picker(&mut ws_b).await;
}

#[tokio::test]
async fn switch_target_returns_to_the_picker_then_reconnects() {
    let vnc_port = spawn_fake_vnc().await;
    let addr = spawn_app(target(Protocol::Vnc, vnc_port)).await;
    let cookie = common::login(addr).await;

    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "test-target").await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws).await;

    // Switch target: disconnect returns the slot to the picker over the same
    // socket (no reclaim, no close).
    ws.send(Message::text(r#"{"type":"disconnect"}"#)).await.unwrap();
    expect_picker(&mut ws).await;

    // Picking again on the same socket starts a fresh engine and repaints.
    common::connect_target(&mut ws, "test-target").await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws).await;
}

#[derive(Debug, PartialEq, Eq)]
struct ClipboardMessage {
    text: String,
    changed_at_ms: Option<u64>,
    requested: bool,
}

/// Read from the socket until a timestamped `clipboard` control message
/// arrives.
async fn expect_clipboard(ws: &mut Ws) -> ClipboardMessage {
    tokio::time::timeout(Duration::from_secs(10), async {
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
                Message::Close(frame) => panic!("closed while waiting for clipboard: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for clipboard");
    })
    .await
    .expect("timed out waiting for clipboard")
}

// The full VNC clipboard round trip over a real socket: what the server cut
// reaches the browser when it asks, and what the browser sends becomes a
// ClientCutText on the wire.
#[tokio::test]
async fn vnc_clipboard_round_trips_when_the_target_opted_in() {
    // Latin-1 above ASCII on the way in (0xE9 is é, one byte on the wire), and
    // a character that has no latin-1 form on the way out — the two encoding
    // edges of RFB cut text.
    let (vnc_port, mut cut_texts) =
        spawn_fake_vnc_with_clipboard(Some(b"copied on caf\xE9")).await;
    let addr = spawn_app(target_with_clipboard(Protocol::Vnc, vnc_port, true)).await;
    let cookie = common::login(addr).await;

    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "test-target").await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;

    // Remote → browser, unprompted: ServerCutText is forwarded as it arrives,
    // which is what drives automatic sync. Deterministic because the fake
    // writes the cut text ahead of the framebuffer update, so it cannot be
    // racing the tile below. The engine decodes latin-1, so the é the server
    // sent as one byte arrives as one character.
    let pushed = expect_clipboard(&mut ws).await;
    assert_eq!(pushed.text, "copied on café");
    assert!(
        pushed.changed_at_ms.is_some(),
        "a live remote clipboard change needs an activity timestamp"
    );
    assert!(!pushed.requested, "a live remote change must remain a push");
    expect_tile(&mut ws).await;

    // And the same text is still there to be fetched: a browser that attached
    // after the push — or reattached — has to be able to ask.
    ws.send(Message::text(r#"{"type":"clipboardRequest"}"#)).await.unwrap();
    let fetched = expect_clipboard(&mut ws).await;
    assert_eq!(fetched.text, pushed.text);
    assert_eq!(
        fetched.changed_at_ms, pushed.changed_at_ms,
        "Fetch must preserve the clipboard activity timestamp"
    );
    assert!(fetched.requested, "Fetch replies must be marked requested");

    // Browser → remote. Latin-1 survives; anything beyond it becomes '?'.
    ws.send(Message::text(r#"{"type":"clipboard","text":"typed ☕ here"}"#))
        .await
        .unwrap();
    let received = tokio::time::timeout(Duration::from_secs(10), cut_texts.recv())
        .await
        .expect("timed out waiting for ClientCutText")
        .expect("cut text channel closed");
    assert_eq!(received, b"typed ? here");

    // An oversized copy reaches the server not at all. Truncating it would hand
    // the remote a paste that looks whole, so the engine drops it and the
    // remote keeps the clipboard it had. The browser refuses this itself and
    // says why; the engine is the belt to that.
    let oversized = "a".repeat(MAX_CLIPBOARD_BYTES + 5_000);
    ws.send(Message::text(format!(
        r#"{{"type":"clipboard","text":"{oversized}"}}"#
    )))
    .await
    .unwrap();
    // Nothing on the wire for it, and — the reason this is worth asserting over
    // the socket rather than in a unit test — the session is still live
    // afterwards: the next copy goes through on the same connection.
    ws.send(Message::text(r#"{"type":"clipboard","text":"after refusal"}"#))
        .await
        .unwrap();
    // The channel is FIFO, so the next thing on it being this proves the
    // oversized copy produced nothing — whole or truncated — and that the
    // refusal cost the session nothing.
    let received = tokio::time::timeout(Duration::from_secs(10), cut_texts.recv())
        .await
        .expect("timed out waiting for ClientCutText")
        .expect("cut text channel closed");
    assert_eq!(received, b"after refusal");
}

// A fetch is answered even when the remote has copied nothing, and the answer
// is empty text rather than silence.
//
// Load-bearing rather than a curiosity: the browser fetches every time the
// clipboard panel is opened and keeps the panel shut until the reply lands, so
// an engine that stayed quiet here would hang the button on every fresh session
// until the client-side timeout expired.
#[tokio::test]
async fn a_fetch_before_the_remote_has_copied_anything_is_still_answered() {
    let (vnc_port, _cut_texts) = spawn_fake_vnc_with_clipboard(None).await;
    let addr = spawn_app(target_with_clipboard(Protocol::Vnc, vnc_port, true)).await;
    let cookie = common::login(addr).await;

    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "test-target").await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    // The tile first, so the engine is demonstrably live and has simply nothing
    // filed rather than not having got there yet.
    expect_tile(&mut ws).await;

    ws.send(Message::text(r#"{"type":"clipboardRequest"}"#)).await.unwrap();
    assert_eq!(
        expect_clipboard(&mut ws).await,
        ClipboardMessage {
            text: String::new(),
            changed_at_ms: None,
            requested: true,
        }
    );
}

// The opt-out path: the flag off means the engine neither answers a fetch nor
// writes to the remote, whatever the browser sends.
#[tokio::test]
async fn vnc_clipboard_is_inert_when_the_target_did_not_opt_in() {
    let (vnc_port, mut cut_texts) = spawn_fake_vnc_with_clipboard(Some(b"secret")).await;
    let addr = spawn_app(target_with_clipboard(Protocol::Vnc, vnc_port, false)).await;
    let cookie = common::login(addr).await;

    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "test-target").await;
    expect_resize(&mut ws, FAKE_DESKTOP, FAKE_DESKTOP).await;
    expect_tile(&mut ws).await;

    ws.send(Message::text(r#"{"type":"clipboardRequest"}"#)).await.unwrap();
    ws.send(Message::text(r#"{"type":"clipboard","text":"leaked"}"#))
        .await
        .unwrap();

    // Nothing may come back, and nothing may reach the server. A refresh acts
    // as the fence: its tile can only arrive after both clipboard messages have
    // been handled, so silence up to that point is silence for good.
    ws.send(Message::text(r#"{"type":"refresh"}"#)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(
                        !text.contains(r#""type":"clipboard""#),
                        "clipboard answered for a target that did not opt in: {text}"
                    );
                }
                Message::Binary(_) => return, // the refresh's tile: the fence
                Message::Close(frame) => panic!("closed unexpectedly: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for the refresh tile");
    })
    .await
    .expect("timed out waiting for the refresh tile");
    assert!(
        cut_texts.try_recv().is_err(),
        "a target that did not opt in must not write the remote's clipboard"
    );
}

/// Read until a `displays` control message arrives, and hand back its payload.
async fn expect_displays(ws: &mut Ws) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            match msg.expect("websocket receive") {
                Message::Text(text) => {
                    assert!(!text.contains(r#""type":"error""#), "session failed: {text}");
                    if text.contains(r#""type":"displays""#) {
                        return serde_json::from_str::<serde_json::Value>(&text).unwrap();
                    }
                }
                Message::Close(frame) => panic!("closed while waiting for displays: {frame:?}"),
                _ => {}
            }
        }
        panic!("websocket ended while waiting for displays");
    })
    .await
    .expect("timed out waiting for a display list")
}

/// The whole `ard-high-performance` wire, end to end: Apple's version banner and
/// DH authentication, the `0xC1` ClientInit, the cleartext prelude, the rekey, and
/// then a display layout, a framebuffer update and a display selection all inside
/// the AES-128-CBC record layer.
///
/// The assertions on the gateway's side of it live in the fake Mac (which panics on
/// a wrong banner, security type, ClientInit byte, prelude shape or display
/// descriptor); what is asserted here is what a *browser* gets out the far end,
/// which is the thing the subtype exists for.
#[tokio::test]
async fn apple_screen_sharing_reports_its_displays_and_binds_to_one() {
    let (mac_port, mut selected) = spawn_fake_mac().await;
    let addr = spawn_app(mac_target(mac_port)).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "test-target").await;

    expect_resize(&mut ws, MAC_DESKTOP, MAC_DESKTOP).await;

    // The display list, which is what standard RFB cannot express at all. Read
    // before the tile because that is the order the Mac sends them in — the layout
    // opens its metadata burst — and `expect_tile` skips text messages on its way
    // past, so asking for the tile first would consume this and then time out.
    let msg = expect_displays(&mut ws).await;
    assert_eq!(msg["active"], MAC_MAIN_DISPLAY, "{msg}");
    let displays = msg["displays"].as_array().expect("a display array");
    assert_eq!(displays.len(), 2, "{msg}");
    assert_eq!(displays[0]["id"], MAC_MAIN_DISPLAY);
    assert_eq!(displays[0]["label"], "Display 1");
    assert_eq!(displays[0]["main"], true);
    assert_eq!(displays[1]["id"], MAC_SECOND_DISPLAY);
    assert_eq!(displays[1]["main"], false);
    // Never a virtual display: this subtype asks the Mac for its own screens.
    assert_eq!(displays[0]["virtual"], false);

    // And the desktop paints, which means every step above it went through: the
    // record layer is up in both directions and RFB is running inside it.
    expect_tile(&mut ws).await;

    // Picking the second screen reaches the Mac as a SetDisplayMessage…
    ws.send(Message::text(format!(
        r#"{{"type":"selectDisplay","id":{MAC_SECOND_DISPLAY}}}"#
    )))
    .await
    .unwrap();
    let asked = tokio::time::timeout(Duration::from_secs(10), selected.recv())
        .await
        .expect("timed out waiting for the Mac to be asked")
        .expect("the selection channel closed");
    assert_eq!(asked, MAC_SECOND_DISPLAY);

    // …and the checkmark follows the Mac's answering layout, not the click.
    let msg = expect_displays(&mut ws).await;
    assert_eq!(msg["active"], MAC_SECOND_DISPLAY, "{msg}");
}

/// A selection of a screen the Mac never listed is dropped rather than forwarded.
///
/// The one case where the gateway is the only thing standing between a client and a
/// Mac being told to bind to something that does not exist — a screen unplugged
/// between the list being sent and the click arriving.
#[tokio::test]
async fn a_display_the_mac_never_listed_is_not_forwarded_to_it() {
    let (mac_port, mut selected) = spawn_fake_mac().await;
    let addr = spawn_app(mac_target(mac_port)).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = connect_ws(addr, &token, &cookie).await;
    common::connect_target(&mut ws, "test-target").await;
    expect_displays(&mut ws).await;

    ws.send(Message::text(r#"{"type":"selectDisplay","id":9999}"#))
        .await
        .unwrap();
    // A real selection after it, which the Mac *does* answer: if the bogus one had
    // been forwarded it would arrive first and this assertion would read it.
    ws.send(Message::text(format!(
        r#"{{"type":"selectDisplay","id":{MAC_SECOND_DISPLAY}}}"#
    )))
    .await
    .unwrap();

    let asked = tokio::time::timeout(Duration::from_secs(10), selected.recv())
        .await
        .expect("timed out waiting for the Mac to be asked")
        .expect("the selection channel closed");
    assert_eq!(asked, MAC_SECOND_DISPLAY, "the unknown display was forwarded");
}
