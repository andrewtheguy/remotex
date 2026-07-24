//! Server-side VNC session: a minimal RFB client (RFC 6143).
//!
//! Guacamole-style baseline (docs/phase2-consolidation.md): protocol 3.8,
//! security None or classic VncAuth, and the **Raw encoding only** — the one
//! encoding every VNC server must support. No per-implementation workarounds:
//! the backend↔VNC hop is LAN, so clever wire encodings buy nothing there;
//! the browser link is optimized by the shared tile transport instead.
//!
//! Mirrors [`crate::rdp`]'s shape behind the [`crate::session`] seam: connect,
//! report the desktop size as [`ServerMsg::Resize`], then pump framebuffer
//! updates out as tiles and browser [`ClientMsg`] input back in.

use std::sync::Arc;

use des::Des;
use des::cipher::generic_array::GenericArray;
use des::cipher::{BlockEncrypt as _, KeyInit as _};
use log::{debug, info, warn};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc};

use crate::config::TargetConfig;
use crate::keymap;
use crate::protocol::{ClientMsg, MouseButton, STRIP_ROWS, ServerMsg, Tile};

const SECURITY_NONE: u8 = 1;
const SECURITY_VNC_AUTH: u8 = 2;
const ENCODING_RAW: i32 = 0;
/// Bytes per pixel of the format we force with SetPixelFormat.
const BPP: usize = 4;
/// Cap on server-sent reason/name strings, so a bogus length can't OOM us.
const MAX_STRING: u32 = 1024;

type Reader = BufReader<OwnedReadHalf>;
type SharedWriter = Arc<Mutex<OwnedWriteHalf>>;

/// Connect to the VNC host, then drive the session until it ends.
///
/// `input_rx` carries browser input; `frame_tx` carries screen updates back.
/// Either closing (browser gone / VNC ended) tears the session down.
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    let (reader, writer, width, height) = match connect(&config).await {
        Ok(v) => v,
        Err(e) => {
            warn!("vnc: connect failed: {e:#}");
            let _ = frame_tx
                .send(ServerMsg::Error {
                    message: format!("VNC connect failed: {e}"),
                })
                .await;
            return;
        }
    };

    info!("vnc: connected, desktop {width}x{height}");
    if frame_tx
        .send(ServerMsg::Resize {
            w: width,
            h: height,
        })
        .await
        .is_err()
    {
        return; // browser already gone
    }

    if let Err(e) = active_loop(reader, writer, (width, height), input_rx, frame_tx.clone()).await
    {
        warn!("vnc: session error: {e:#}");
        let _ = frame_tx
            .send(ServerMsg::Error {
                message: format!("VNC session ended: {e}"),
            })
            .await;
    }
    info!("vnc: session terminated");
}

/// TCP connect → RFB version/security handshake → ClientInit/ServerInit →
/// force our pixel format and the raw-only encoding set.
async fn connect(
    config: &TargetConfig,
) -> anyhow::Result<(Reader, OwnedWriteHalf, u16, u16)> {
    let dest = host_port(&config.host, config.port);
    let stream = TcpStream::connect(&dest)
        .await
        .map_err(|e| anyhow::anyhow!("TCP connect to {dest}: {e}"))?;
    stream.set_nodelay(true).ok();
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Version handshake. The server leads with e.g. "RFB 003.008\n"; anything
    // announcing at least 3.8 (macOS Screen Sharing says 3.889) is answered
    // with 3.8, the baseline this client speaks.
    let mut greeting = [0u8; 12];
    reader.read_exact(&mut greeting).await?;
    let (major, minor) =
        parse_version(&greeting).ok_or_else(|| anyhow::anyhow!("not an RFB server: {greeting:?}"))?;
    anyhow::ensure!(
        major > 3 || (major == 3 && minor >= 8),
        "unsupported RFB version {major}.{minor} (this client requires 3.8+)"
    );
    writer.write_all(b"RFB 003.008\n").await?;

    // Security handshake (3.8 style): the server lists types, we pick one.
    let type_count = reader.read_u8().await?;
    if type_count == 0 {
        anyhow::bail!(
            "VNC server refused the connection: {}",
            read_string(&mut reader).await?
        );
    }
    let mut types = vec![0u8; usize::from(type_count)];
    reader.read_exact(&mut types).await?;

    let chosen = if !config.password.is_empty() && types.contains(&SECURITY_VNC_AUTH) {
        SECURITY_VNC_AUTH
    } else if types.contains(&SECURITY_NONE) {
        SECURITY_NONE
    } else if types.contains(&SECURITY_VNC_AUTH) {
        anyhow::bail!("VNC server requires a password but the target has none configured");
    } else {
        anyhow::bail!(
            "no supported VNC security type (server offers {types:?}; \
             this client speaks None and VncAuth only)"
        );
    };
    writer.write_all(&[chosen]).await?;

    if chosen == SECURITY_VNC_AUTH {
        let mut challenge = [0u8; 16];
        reader.read_exact(&mut challenge).await?;
        writer
            .write_all(&auth_response(&config.password, &challenge))
            .await?;
    }

    // SecurityResult (sent for every type in 3.8, including None).
    if reader.read_u32().await? != 0 {
        anyhow::bail!(
            "VNC authentication failed: {}",
            read_string(&mut reader).await?
        );
    }

    // ClientInit: request a shared session (don't kick other clients; the
    // single-session policy lives in this program, not on the VNC server).
    writer.write_all(&[1]).await?;

    // ServerInit: desktop size, the server's native pixel format (ignored —
    // we override it), and the desktop name.
    let width = reader.read_u16().await?;
    let height = reader.read_u16().await?;
    let mut native_format = [0u8; 16];
    reader.read_exact(&mut native_format).await?;
    let name = read_string(&mut reader).await?;
    debug!("vnc: server desktop {name:?}");
    anyhow::ensure!(width > 0 && height > 0, "server reported a {width}x{height} desktop");

    writer.write_all(&set_pixel_format()).await?;
    writer.write_all(&set_encodings(&[ENCODING_RAW])).await?;

    Ok((reader, writer, width, height))
}

/// Drive the active session: framebuffer updates out, browser input in.
async fn active_loop(
    reader: Reader,
    writer: OwnedWriteHalf,
    size: (u16, u16),
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    // The writer is shared: the read loop sends the next update request after
    // each update, the input side sends pointer/key events.
    let writer: SharedWriter = Arc::new(Mutex::new(writer));

    // Kick off the update cycle with one full (non-incremental) request.
    write_to(&writer, &update_request(false, size)).await?;

    let mut read_task = tokio::spawn(read_loop(reader, Arc::clone(&writer), size, frame_tx));

    // RFB pointer events always carry position + full button mask, so both are
    // tracked across browser events (which report only the changed part).
    let mut button_mask = 0u8;
    let mut last_pos = (size.0 / 2, size.1 / 2);

    let result = loop {
        tokio::select! {
            res = &mut read_task => {
                return res.map_err(|e| anyhow::anyhow!("read task failed: {e}"))?;
            }
            input = input_rx.recv() => {
                let Some(input) = input else {
                    info!("vnc: input channel closed by browser");
                    break Ok(());
                };
                let bytes = translate_input(input, &mut button_mask, &mut last_pos);
                if !bytes.is_empty() {
                    write_to(&writer, &bytes).await?;
                }
            }
        }
    };
    read_task.abort();
    result
}

/// Read server messages forever, forwarding framebuffer updates as tiles.
async fn read_loop(
    mut reader: Reader,
    writer: SharedWriter,
    size: (u16, u16),
    frame_tx: mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    loop {
        let msg_type = match reader.read_u8().await {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                info!("vnc: server closed the connection");
                return Ok(());
            }
            Err(e) => return Err(anyhow::anyhow!("read server message: {e}")),
        };
        match msg_type {
            // FramebufferUpdate
            0 => {
                reader.read_u8().await?; // padding
                let rects = reader.read_u16().await?;
                for _ in 0..rects {
                    read_rect(&mut reader, size, &frame_tx).await?;
                }
                // Complete the cycle: ask for the next incremental update.
                write_to(&writer, &update_request(true, size)).await?;
            }
            // SetColourMapEntries — can't happen for the true-colour format we
            // set, but consume it correctly rather than desyncing the stream.
            1 => {
                reader.read_u8().await?; // padding
                reader.read_u16().await?; // first colour index
                let colours = reader.read_u16().await?;
                discard(&mut reader, u64::from(colours) * 6).await?;
            }
            // Bell — nothing to ring in the browser (yet).
            2 => {}
            // ServerCutText — clipboard is a later phase; drain and drop.
            3 => {
                let mut padding = [0u8; 3];
                reader.read_exact(&mut padding).await?;
                let len = reader.read_u32().await?;
                discard(&mut reader, u64::from(len)).await?;
            }
            other => anyhow::bail!("unknown server message type {other}"),
        }
    }
}

/// Read one FramebufferUpdate rectangle (raw encoding only) and forward it as
/// PNG tiles, split into [`STRIP_ROWS`] strips like the RDP engine.
async fn read_rect(
    reader: &mut Reader,
    size: (u16, u16),
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    let x = reader.read_u16().await?;
    let y = reader.read_u16().await?;
    let w = reader.read_u16().await?;
    let h = reader.read_u16().await?;
    let encoding = reader.read_i32().await?;
    anyhow::ensure!(
        encoding == ENCODING_RAW,
        "server sent encoding {encoding}, but only raw ({ENCODING_RAW}) was advertised"
    );
    // Bounds-check before allocating: a rect outside the announced desktop is
    // a protocol violation (and would let a bad length drive the allocation).
    anyhow::ensure!(
        u32::from(x) + u32::from(w) <= u32::from(size.0)
            && u32::from(y) + u32::from(h) <= u32::from(size.1),
        "rect {w}x{h}+{x}+{y} exceeds the {}x{} desktop",
        size.0,
        size.1
    );
    if w == 0 || h == 0 {
        return Ok(());
    }

    let mut pixels = vec![0u8; usize::from(w) * usize::from(h) * BPP];
    reader.read_exact(&mut pixels).await?;

    let mut done = 0u16;
    while done < h {
        let strip_h = STRIP_ROWS.min(h - done);
        let start = usize::from(done) * usize::from(w) * BPP;
        let end = start + usize::from(strip_h) * usize::from(w) * BPP;
        let rgb = bgrx_to_rgb(&pixels[start..end]);
        let tile = Tile::from_rgb(x, y + done, w, strip_h, &rgb)?;
        debug!(
            "vnc: tile {w}x{strip_h} at ({x},{}): {} -> {} bytes",
            y + done,
            end - start,
            tile.data.len()
        );
        frame_tx
            .send(ServerMsg::Tile(tile))
            .await
            .map_err(|_| anyhow::anyhow!("frame channel closed"))?;
        done += strip_h;
    }
    Ok(())
}

/// Translate one browser input message into RFB client messages, updating the
/// tracked pointer state.
fn translate_input(
    input: ClientMsg,
    button_mask: &mut u8,
    last_pos: &mut (u16, u16),
) -> Vec<u8> {
    match input {
        ClientMsg::MouseMove { x, y } => {
            *last_pos = (clamp_u16(x), clamp_u16(y));
            pointer_event(*button_mask, *last_pos).to_vec()
        }
        ClientMsg::MouseButton { button, pressed } => {
            let bit = match button {
                MouseButton::Left => 0x01,
                MouseButton::Middle => 0x02,
                MouseButton::Right => 0x04,
            };
            if pressed {
                *button_mask |= bit;
            } else {
                *button_mask &= !bit;
            }
            pointer_event(*button_mask, *last_pos).to_vec()
        }
        ClientMsg::Wheel { dx, dy } => {
            // A wheel notch is a press+release of buttons 4-7 (mask bits 3-6):
            // 4 = up, 5 = down, 6 = left, 7 = right. One notch per event,
            // like the RDP engine.
            let mut out = Vec::new();
            for (delta, negative_bit, positive_bit) in [(dy, 0x08, 0x10), (dx, 0x20, 0x40)] {
                if delta != 0.0 {
                    let bit = if delta > 0.0 { positive_bit } else { negative_bit };
                    out.extend_from_slice(&pointer_event(*button_mask | bit, *last_pos));
                    out.extend_from_slice(&pointer_event(*button_mask, *last_pos));
                }
            }
            out
        }
        ClientMsg::Key { code, pressed } => match keymap::keysym(&code) {
            Some(sym) => key_event(pressed, sym).to_vec(),
            None => {
                debug!("vnc: unmapped key code {code}");
                Vec::new()
            }
        },
    }
}

// ── RFB message builders (all integers big-endian, per RFC 6143) ────────────

/// SetPixelFormat: 32 bpp, depth 24, little-endian, true colour, 8 bits per
/// channel with red<<16 / green<<8 / blue<<0 — i.e. raw pixels arrive as
/// B, G, R, pad bytes, which [`bgrx_to_rgb`] repacks for the tile encoder.
fn set_pixel_format() -> [u8; 20] {
    let mut msg = [0u8; 20];
    msg[0] = 0; // message type
    // msg[1..4]: padding
    msg[4] = 32; // bits per pixel
    msg[5] = 24; // depth
    msg[6] = 0; // big-endian flag: off
    msg[7] = 1; // true-colour flag: on
    msg[8..10].copy_from_slice(&255u16.to_be_bytes()); // red max
    msg[10..12].copy_from_slice(&255u16.to_be_bytes()); // green max
    msg[12..14].copy_from_slice(&255u16.to_be_bytes()); // blue max
    msg[14] = 16; // red shift
    msg[15] = 8; // green shift
    msg[16] = 0; // blue shift
    // msg[17..20]: padding
    msg
}

/// SetEncodings for the given encoding list.
fn set_encodings(encodings: &[i32]) -> Vec<u8> {
    let mut msg = vec![2u8, 0];
    msg.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for &encoding in encodings {
        msg.extend_from_slice(&encoding.to_be_bytes());
    }
    msg
}

/// FramebufferUpdateRequest for the whole desktop.
fn update_request(incremental: bool, size: (u16, u16)) -> [u8; 10] {
    let mut msg = [0u8; 10];
    msg[0] = 3; // message type
    msg[1] = u8::from(incremental);
    // msg[2..6]: x, y = 0
    msg[6..8].copy_from_slice(&size.0.to_be_bytes());
    msg[8..10].copy_from_slice(&size.1.to_be_bytes());
    msg
}

/// KeyEvent.
fn key_event(down: bool, keysym: u32) -> [u8; 8] {
    let mut msg = [0u8; 8];
    msg[0] = 4; // message type
    msg[1] = u8::from(down);
    // msg[2..4]: padding
    msg[4..8].copy_from_slice(&keysym.to_be_bytes());
    msg
}

/// PointerEvent.
fn pointer_event(button_mask: u8, pos: (u16, u16)) -> [u8; 6] {
    let mut msg = [0u8; 6];
    msg[0] = 5; // message type
    msg[1] = button_mask;
    msg[2..4].copy_from_slice(&pos.0.to_be_bytes());
    msg[4..6].copy_from_slice(&pos.1.to_be_bytes());
    msg
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Classic VNC authentication: DES-ECB over the 16-byte challenge, keyed by
/// the first 8 bytes of the password (zero-padded) with the bit order of each
/// key byte reversed — the RFB spec's non-standard DES key convention.
fn auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes()) {
        *slot = byte.reverse_bits();
    }
    let cipher = Des::new(GenericArray::from_slice(&key));
    let mut response = *challenge;
    for block in response.chunks_exact_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    response
}

/// Parse the 12-byte RFB greeting `RFB xxx.yyy\n` into (major, minor).
fn parse_version(greeting: &[u8; 12]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(greeting).ok()?;
    let rest = text.strip_prefix("RFB ")?.strip_suffix('\n')?;
    let (major, minor) = rest.split_once('.')?;
    if major.len() != 3 || minor.len() != 3 {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Repack BGRX pixels (our forced format on the wire) into packed RGB888.
fn bgrx_to_rgb(bgrx: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(bgrx.len() / BPP * 3);
    for px in bgrx.chunks_exact(BPP) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    rgb
}

/// Read a u32-length-prefixed latin-1 string (reason or desktop name),
/// truncated to [`MAX_STRING`] with the excess drained off the stream.
async fn read_string(reader: &mut Reader) -> anyhow::Result<String> {
    let len = reader.read_u32().await?;
    let keep = len.min(MAX_STRING);
    let mut buf = vec![0u8; keep as usize];
    reader.read_exact(&mut buf).await?;
    discard(reader, u64::from(len - keep)).await?;
    Ok(buf.iter().map(|&b| char::from(b)).collect())
}

/// Drain and drop exactly `n` bytes.
async fn discard(reader: &mut Reader, n: u64) -> anyhow::Result<()> {
    let copied = tokio::io::copy(&mut reader.take(n), &mut tokio::io::sink()).await?;
    anyhow::ensure!(copied == n, "connection closed while skipping {n} bytes");
    Ok(())
}

async fn write_to(writer: &SharedWriter, bytes: &[u8]) -> anyhow::Result<()> {
    writer
        .lock()
        .await
        .write_all(bytes)
        .await
        .map_err(|e| anyhow::anyhow!("write to VNC server: {e}"))
}

fn clamp_u16(v: i32) -> u16 {
    v.clamp(0, i32::from(u16::MAX)) as u16
}

/// Format a `host:port` destination, bracketing bare IPv6 literals.
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors generated from remotex's known-good vncAuth implementation
    // (server/vncAuth.ts, node:crypto des-ecb) with the challenge 00 01 .. 0f.
    #[test]
    fn auth_response_matches_reference_implementation() {
        let challenge: [u8; 16] = std::array::from_fn(|i| i as u8);
        let cases = [
            ("secret42", "c6e31ed26154432307b32f3f00a3e6a1"),
            // Longer than 8 bytes: only the first 8 are used.
            ("longpassword", "5931256585fd62106d317e09fc963baf"),
            // Shorter than 8 bytes: zero-padded.
            ("ab", "fe01155de95da3e28adf6cc730f06f08"),
        ];
        for (password, expected_hex) in cases {
            let response = auth_response(password, &challenge);
            let hex: String = response.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(hex, expected_hex, "password {password:?}");
        }
    }

    #[test]
    fn auth_response_truncation_boundary() {
        // "longpass" and "longpassword" share the first 8 bytes, so their
        // responses must be identical; a 9th significant byte would differ.
        let challenge = [7u8; 16];
        assert_eq!(
            auth_response("longpass", &challenge),
            auth_response("longpassword", &challenge)
        );
        assert_ne!(
            auth_response("longpas", &challenge),
            auth_response("longpass", &challenge)
        );
    }

    #[test]
    fn version_parses_and_gates() {
        assert_eq!(parse_version(b"RFB 003.008\n"), Some((3, 8)));
        assert_eq!(parse_version(b"RFB 003.889\n"), Some((3, 889))); // macOS
        assert_eq!(parse_version(b"RFB 004.001\n"), Some((4, 1))); // RealVNC
        assert_eq!(parse_version(b"HTTP/1.1 200"), None);
        assert_eq!(parse_version(b"RFB 03.008\n\n"), None);
    }

    #[test]
    fn pixel_format_is_bgrx_little_endian_true_colour() {
        let msg = set_pixel_format();
        assert_eq!(msg[0], 0);
        assert_eq!((msg[4], msg[5]), (32, 24)); // bpp, depth
        assert_eq!((msg[6], msg[7]), (0, 1)); // little-endian, true-colour
        assert_eq!(&msg[8..14], &[0, 255, 0, 255, 0, 255]); // maxima
        assert_eq!(&msg[14..17], &[16, 8, 0]); // shifts
    }

    #[test]
    fn bgrx_repacks_to_rgb() {
        // Two pixels: pure red and pure blue in BGRX order.
        let bgrx = [0, 0, 255, 0, 255, 0, 0, 0];
        assert_eq!(bgrx_to_rgb(&bgrx), vec![255, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn raw_only_encoding_set() {
        assert_eq!(set_encodings(&[ENCODING_RAW]), vec![2, 0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn update_request_covers_the_desktop() {
        assert_eq!(
            update_request(true, (1280, 800)),
            [3, 1, 0, 0, 0, 0, 0x05, 0x00, 0x03, 0x20]
        );
        assert_eq!(update_request(false, (1, 1))[1], 0);
    }

    #[test]
    fn pointer_and_key_events_encode_big_endian() {
        assert_eq!(pointer_event(0x05, (0x0102, 0x0304)), [5, 5, 1, 2, 3, 4]);
        assert_eq!(key_event(true, 0xFF0D), [4, 1, 0, 0, 0, 0, 0xFF, 0x0D]);
        assert_eq!(key_event(false, 0x61), [4, 0, 0, 0, 0, 0, 0, 0x61]);
    }

    #[test]
    fn buttons_accumulate_in_the_mask_and_wheel_pulses() {
        let mut mask = 0u8;
        let mut pos = (10, 20);

        let bytes = translate_input(
            ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            &mut mask,
            &mut pos,
        );
        assert_eq!(bytes, pointer_event(0x01, (10, 20)).to_vec());

        // A move while the button is held keeps it in the mask (drag).
        let bytes = translate_input(ClientMsg::MouseMove { x: 30, y: 40 }, &mut mask, &mut pos);
        assert_eq!(bytes, pointer_event(0x01, (30, 40)).to_vec());

        // Scroll down = button 5 (0x10) press + release, on top of the held mask.
        let bytes = translate_input(ClientMsg::Wheel { dx: 0.0, dy: 3.0 }, &mut mask, &mut pos);
        let mut expected = pointer_event(0x11, (30, 40)).to_vec();
        expected.extend_from_slice(&pointer_event(0x01, (30, 40)));
        assert_eq!(bytes, expected);

        let bytes = translate_input(
            ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            },
            &mut mask,
            &mut pos,
        );
        assert_eq!(bytes, pointer_event(0x00, (30, 40)).to_vec());
    }

    #[test]
    fn key_input_maps_to_keysyms_and_drops_unknown_codes() {
        let (mut mask, mut pos) = (0u8, (0u16, 0u16));
        assert_eq!(
            translate_input(
                ClientMsg::Key {
                    code: "KeyA".to_owned(),
                    pressed: true
                },
                &mut mask,
                &mut pos
            ),
            key_event(true, 0x61).to_vec()
        );
        assert!(
            translate_input(
                ClientMsg::Key {
                    code: "MediaPlayPause".to_owned(),
                    pressed: true
                },
                &mut mask,
                &mut pos
            )
            .is_empty()
        );
    }
}
