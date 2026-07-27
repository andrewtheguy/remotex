//! Server-side VNC session: a minimal RFB client (RFC 6143).
//!
//! Guacamole-style baseline (docs/architecture.md): protocol 3.8,
//! security None or classic VncAuth, and the **Raw encoding only** — the one
//! encoding every VNC server must support. No per-implementation workarounds:
//! the backend↔VNC hop is LAN, so clever wire encodings buy nothing there;
//! the browser link is optimized by the shared tile transport instead.
//!
//! On top of the baseline, the **Cursor pseudo-encoding** is always
//! advertised: a server that supports it stops compositing the pointer into
//! the framebuffer and sends the shape instead, which the browser draws
//! locally ([`ServerMsg::Cursor`]). This is what makes the pointer visible on
//! servers that never composited it in the first place — macOS Screen Sharing
//! draws no cursor into the framebuffer at all, so without this the desktop
//! arrives with no pointer anywhere on it.
//!
//! On top of the baseline, **dynamic resize** is available per
//! target opt-in (`resize = true`): the DesktopSize/ExtendedDesktopSize
//! pseudo-encodings are advertised, and browser viewport reports
//! ([`ClientMsg::Viewport`]) become `SetDesktopSize` requests once the server
//! declares support, so TigerVNC-family servers render at the browser's size.
//! Servers (or targets) without it keep the connect-time size.
//!
//! Also per target opt-in (`clipboard = true`): the **cut text** messages.
//! `ServerCutText` fills a buffer the browser fetches on demand, and the
//! browser's sends become `ClientCutText`. Baseline only — the Extended
//! Clipboard pseudo-encoding (UTF-8, zlib, a capability handshake) is not
//! negotiated, so this text is latin-1 and anything outside it becomes `?`.
//!
//! Mirrors [`crate::rdp`]'s shape behind the [`crate::session`] seam: connect,
//! report the desktop size as [`ServerMsg::Resize`], then pump framebuffer
//! updates out as tiles and browser [`ClientMsg`] input back in.

use std::collections::HashMap;
use std::sync::Arc;

use des::Des;
use des::cipher::generic_array::GenericArray;
use des::cipher::{BlockEncrypt as _, KeyInit as _};
use log::{debug, info, warn};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc};

use crate::config::TargetConfig;
use crate::engine::{clamp_u16, host_port};
use crate::keymap;
use crate::protocol::{
    ClientMsg, ClipboardSnapshot, CursorShape, MAX_CLIPBOARD_BYTES, MouseButton, STRIP_ROWS,
    ServerMsg, Tile, UNSCALED, clipboard_fits,
};
use crate::vnc_clipboard;

const SECURITY_NONE: u8 = 1;
const SECURITY_VNC_AUTH: u8 = 2;
const ENCODING_RAW: i32 = 0;
/// Cursor pseudo-encoding: the server hands over the pointer shape (pixels +
/// a 1-bit mask, the rect's x/y being the hotspot) instead of drawing it into
/// the framebuffer.
const ENCODING_CURSOR: i32 = -239;
/// DesktopSize pseudo-encoding: the server announces a new framebuffer size.
const ENCODING_DESKTOP_SIZE: i32 = -223;
/// ExtendedDesktopSize pseudo-encoding: size announcements with a screen
/// layout, and the server's declaration that it accepts SetDesktopSize.
const ENCODING_EXTENDED_DESKTOP_SIZE: i32 = -308;
/// Bytes per pixel of the format we force with SetPixelFormat.
const BPP: usize = 4;
/// Cap on server-sent reason/name strings, so a bogus length can't OOM us.
const MAX_STRING: u32 = 1024;
/// Largest cursor edge accepted. Real pointers are 32x32 or 64x64; anything
/// beyond this is drained and ignored rather than drawn.
const MAX_CURSOR_DIM: u16 = 256;

type Reader = BufReader<OwnedReadHalf>;
type SharedWriter = Arc<Mutex<OwnedWriteHalf>>;

/// One screen in the server's ExtendedDesktopSize layout. Only the id and
/// flags matter here: SetDesktopSize echoes them back with new dimensions.
#[derive(Debug, Clone, Copy)]
struct Screen {
    id: u32,
    flags: u32,
}

/// Desktop geometry, shared between the read loop (which learns about
/// resizes and server support) and the input side (which requests them).
/// The lock is never held across an await.
#[derive(Debug)]
struct DesktopState {
    /// Current framebuffer size.
    size: (u16, u16),
    /// First screen of the server's layout. `Some` only once the server has
    /// sent an ExtendedDesktopSize rect — its declaration that SetDesktopSize
    /// is supported; nothing is requested before that.
    screen: Option<Screen>,
    /// A browser viewport report that arrived before support was declared,
    /// replayed on the first ExtendedDesktopSize rect.
    pending: Option<(u16, u16)>,
}

type SharedDesktop = Arc<std::sync::Mutex<DesktopState>>;

/// What the browser should draw for the pointer, tracked so a browser that
/// (re)attaches mid-session gets it replayed — the server only sends the shape
/// when it changes, which may have been long before this browser showed up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum CursorState {
    /// No Cursor rect has arrived: the server is compositing the pointer into
    /// the framebuffer itself, so the browser must not draw one.
    #[default]
    ServerDrawn,
    /// The server owns the shape and has currently hidden the pointer.
    Hidden,
    /// The latest shape the server sent.
    Shape(CursorShape),
}

type SharedCursor = Arc<std::sync::Mutex<CursorState>>;

/// Both ends of the clipboard bridge, shared between the read loop (which
/// fills `remote` and learns the server's capabilities) and the input side
/// (which answers a Fetch from `remote` and records `local`).
///
/// RFB has no "read the clipboard" request — the server pushes whenever the
/// remote clipboard changes — so `remote` keeps the latest text to answer
/// [`ClientMsg::ClipboardRequest`]. Forwarding the push live is not enough on
/// its own: a browser that attaches mid-session, or reattaches after a drop,
/// has missed every push so far and would see an empty panel with no way to
/// ask.
#[derive(Debug, Default)]
struct ClipboardState {
    /// What the remote last sent. `None` means nothing has been copied there
    /// this session.
    remote: Option<ClipboardSnapshot>,
    /// What the browser last sent, held until the server asks for it. Only the
    /// extended path defers like that; the latin-1 fallback writes immediately
    /// and never reads this.
    local: Option<String>,
    /// What the server said it can do, from its Extended Clipboard caps.
    /// `None` until caps arrive, which is also how "the server does not speak
    /// the extension, use latin-1" is spelled — see [`crate::vnc_clipboard`].
    server: Option<vnc_clipboard::Caps>,
}

type SharedClipboard = Arc<std::sync::Mutex<ClipboardState>>;

/// Connect to the VNC host, then drive the session until it ends.
///
/// `input_rx` carries browser input; `frame_tx` carries screen updates back.
/// Either closing (browser gone / VNC ended) tears the session down.
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    let connected = match connect(&config).await {
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

    let Connected { reader, writer, width, height, macos } = connected;
    info!("vnc: connected, desktop {width}x{height} (macos={macos})");
    if frame_tx
        .send(ServerMsg::Resize {
            w: width,
            h: height,
            scale: UNSCALED,
        })
        .await
        .is_err()
    {
        return; // browser already gone
    }
    if frame_tx.send(ServerMsg::RemoteOs { macos }).await.is_err() {
        return; // browser already gone
    }

    if let Err(e) = active_loop(
        reader,
        writer,
        (width, height),
        Flags { macos, resize: config.resize, clipboard: config.clipboard },
        input_rx,
        frame_tx.clone(),
    )
    .await
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

/// The per-session switches [`active_loop`] needs: one discovered from the
/// handshake, two opted into by the target profile.
struct Flags {
    macos: bool,
    resize: bool,
    clipboard: bool,
}

/// An established, handshaken RFB link, plus what the handshake revealed about
/// the far side. Mirrors [`crate::rxa::Session`] rather than returning a tuple
/// nobody can read at the call site.
struct Connected {
    reader: Reader,
    writer: OwnedWriteHalf,
    width: u16,
    height: u16,
    /// Whether the server is macOS Screen Sharing — see [`is_macos_server`].
    macos: bool,
}

/// TCP connect → RFB version/security handshake → ClientInit/ServerInit →
/// force our pixel format and the encoding set (raw + the resize
/// pseudo-encodings).
async fn connect(config: &TargetConfig) -> anyhow::Result<Connected> {
    let dest = host_port(&config.host, config.port);
    let stream = crate::engine::tcp_connect(&dest).await?;
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
    let macos = is_macos_server(minor, &types);

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
    // Cursor is unconditional (the browser can always draw a pointer). The
    // resize pseudo-encodings are advertised only when the target opts in
    // (`resize = true`); without them the server never announces support and
    // the desktop keeps its connect-time size.
    let mut encodings = vec![ENCODING_RAW, ENCODING_CURSOR];
    if config.resize {
        encodings.push(ENCODING_EXTENDED_DESKTOP_SIZE);
        encodings.push(ENCODING_DESKTOP_SIZE);
    }
    if config.clipboard {
        // Extended Clipboard, which is the only way RFB carries anything
        // outside latin-1. Advertised on opt-in only; a server that ignores it
        // simply never sends caps and the latin-1 path stays in use.
        encodings.push(vnc_clipboard::ENCODING);
    }
    writer.write_all(&set_encodings(&encodings)).await?;

    Ok(Connected {
        reader,
        writer,
        width,
        height,
        macos,
    })
}

/// Drive the active session: framebuffer updates out, browser input in.
async fn active_loop(
    reader: Reader,
    writer: OwnedWriteHalf,
    size: (u16, u16),
    flags: Flags,
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    let Flags { macos, resize, clipboard: clipboard_enabled } = flags;
    // The writer is shared: the read loop sends the next update request after
    // each update, the input side sends pointer/key/resize messages.
    let writer: SharedWriter = Arc::new(Mutex::new(writer));
    let desktop: SharedDesktop = Arc::new(std::sync::Mutex::new(DesktopState {
        size,
        screen: None,
        pending: None,
    }));
    let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
    let clipboard: SharedClipboard = Arc::new(std::sync::Mutex::new(ClipboardState::default()));

    // Kick off the update cycle with one full (non-incremental) request.
    write_to(&writer, &update_request(false, size)).await?;

    let mut read_task = tokio::spawn(read_loop(
        reader,
        Arc::clone(&writer),
        Arc::clone(&desktop),
        Arc::clone(&cursor),
        Arc::clone(&clipboard),
        clipboard_enabled,
        frame_tx.clone(),
    ));

    // RFB pointer events always carry position + full button mask, so both are
    // tracked across browser events (which report only the changed part).
    let mut button_mask = 0u8;
    let mut last_pos = (size.0 / 2, size.1 / 2);
    // The keysym actually sent for each pressed DOM code, so a key released
    // after Shift is let go still releases the shifted keysym it was pressed
    // with (down/up symmetry). Doubles as the live Shift state. CapsLock is not
    // tracked here — every key event carries the browser's authoritative lock
    // state (see [`ClientMsg::Key`]).
    let mut pressed_keys: HashMap<String, u32> = HashMap::new();

    let result = loop {
        tokio::select! {
            res = &mut read_task => {
                return res.map_err(|e| anyhow::anyhow!("read task failed: {e}"))?;
            }
            input = input_rx.recv() => {
                let Some(input) = input else {
                    info!("vnc: input channel closed; session shut down");
                    break Ok(());
                };
                // Viewport reports drive dynamic resize, not an input event;
                // dropped entirely unless the target opted in.
                let sent = if let ClientMsg::Viewport { w, h } = input {
                    if resize {
                        request_resize(&writer, &desktop, (w, h)).await
                    } else {
                        Ok(())
                    }
                } else if matches!(input, ClientMsg::Refresh) {
                    // A (re)attached browser needs the desktop size and a full
                    // repaint. Unlike RDP, this engine keeps no
                    // framebuffer copy — the VNC server is one LAN hop away,
                    // so a non-incremental update request repaints just as
                    // well without duplicating the framebuffer here.
                    let size = desktop.lock().unwrap().size;
                    if frame_tx
                        .send(ServerMsg::Resize {
                            w: size.0,
                            h: size.1,
                            scale: UNSCALED,
                        })
                        .await
                        .is_err()
                    {
                        break Err(anyhow::anyhow!("frame channel closed"));
                    }
                    if frame_tx.send(ServerMsg::RemoteOs { macos }).await.is_err() {
                        break Err(anyhow::anyhow!("frame channel closed"));
                    }
                    // The pointer shape is not part of a repaint — the server
                    // resends it only when it changes — so replay the cached
                    // one, or the fresh browser would draw no pointer at all.
                    if let Some(msg) = cursor_msg(&cursor)
                        && frame_tx.send(msg).await.is_err()
                    {
                        break Err(anyhow::anyhow!("frame channel closed"));
                    }
                    write_to(&writer, &update_request(false, size)).await
                } else if matches!(input, ClientMsg::ClipboardRequest) {
                    // Answered from the buffer the read loop fills: RFB has no
                    // way to *ask* the server for its clipboard. Empty until
                    // the remote copies something, which reads in the panel as
                    // "nothing has been copied over there yet".
                    if clipboard_enabled {
                        let snapshot = clipboard
                            .lock()
                            .unwrap()
                            .remote
                            .clone()
                            .unwrap_or_else(ClipboardSnapshot::unobserved);
                        if frame_tx
                            .send(ServerMsg::Clipboard {
                                text: snapshot.text,
                                changed_at_ms: snapshot.changed_at_ms,
                                requested: true,
                                oversized_bytes: snapshot.oversized_bytes,
                            })
                            .await
                            .is_err()
                        {
                            break Err(anyhow::anyhow!("frame channel closed"));
                        }
                    }
                    Ok(())
                } else if let ClientMsg::Clipboard { text } = &input {
                    if clipboard_enabled && !clipboard_fits(text) {
                        // Refused, as the RDP and rxa engines do: the remote
                        // keeps what it had rather than being handed a partial
                        // copy that looks whole. Also keeps an oversized string
                        // out of `state.local`, which the deferred Provide can
                        // be asked for long after the copy.
                        warn!(
                            "vnc: refusing {} bytes to the remote clipboard, over the {MAX_CLIPBOARD_BYTES} byte limit",
                            text.len()
                        );
                        Ok(())
                    } else if clipboard_enabled {
                        // Extended when the server offered it, which is the
                        // only path that carries anything outside latin-1.
                        // Deferred by design: advertise now, hand the text over
                        // when the remote actually pastes and asks for it.
                        let extended = {
                            let mut state = clipboard.lock().unwrap();
                            state.local = Some(text.to_owned());
                            state
                                .server
                                .is_some_and(|caps| caps.handles(vnc_clipboard::ACTION_NOTIFY))
                        };
                        if extended {
                            let notify = vnc_clipboard::notify(vnc_clipboard::FORMAT_TEXT);
                            write_to(&writer, &cut_text_extended(&notify)).await
                        } else {
                            // Unreachable None: the branch above refused
                            // anything over the ceiling.
                            match client_cut_text(text) {
                                Some(msg) => write_to(&writer, &msg).await,
                                None => Ok(()),
                            }
                        }
                    } else {
                        Ok(())
                    }
                } else {
                    match translate_input(input, &mut button_mask, &mut last_pos, &mut pressed_keys) {
                        bytes if bytes.is_empty() => Ok(()),
                        bytes => write_to(&writer, &bytes).await,
                    }
                };
                // Break instead of `?`: the error must pass the trailing
                // read_task.abort() on its way out.
                if let Err(e) = sent {
                    break Err(e);
                }
            }
        }
    };
    read_task.abort();
    result
}

/// Handle a browser viewport report (dynamic resize): send
/// SetDesktopSize once the server has declared support via an
/// ExtendedDesktopSize rect; until then, stash the report for replay.
async fn request_resize<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<W>>,
    desktop: &SharedDesktop,
    want: (u16, u16),
) -> anyhow::Result<()> {
    let msg = {
        let mut d = desktop.lock().unwrap();
        if want.0 == 0 || want.1 == 0 {
            return Ok(());
        }
        if want == d.size {
            // The browser is back at the current size; drop any stale stash
            // so a later support declaration doesn't replay it.
            d.pending = None;
            return Ok(());
        }
        match d.screen {
            Some(screen) => set_desktop_size(want, screen),
            None => {
                d.pending = Some(want);
                return Ok(());
            }
        }
    };
    debug!("vnc: requesting desktop resize to {}x{}", want.0, want.1);
    write_to(writer, &msg).await
}

/// Read server messages forever, forwarding framebuffer updates as tiles.
async fn read_loop(
    mut reader: Reader,
    writer: SharedWriter,
    desktop: SharedDesktop,
    cursor: SharedCursor,
    clipboard: SharedClipboard,
    clipboard_enabled: bool,
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
                let mut resized = false;
                for _ in 0..rects {
                    resized |= read_rect(&mut reader, &writer, &desktop, &cursor, &frame_tx).await?;
                }
                // Complete the cycle. A resize invalidates the old contents,
                // so repaint fully; otherwise ask for the next increment.
                let size = desktop.lock().unwrap().size;
                write_to(&writer, &update_request(!resized, size)).await?;
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
            // ServerCutText — the remote's clipboard changed. Pushed to the
            // browser as it arrives *and* stashed, because the two serve
            // different readers: the push drives automatic sync, the stash
            // answers a Fetch from a browser that attached later and so never
            // saw the push. Drained and dropped when the target didn't opt in.
            3 => {
                let mut padding = [0u8; 3];
                reader.read_exact(&mut padding).await?;
                // Signed: a negative length marks an Extended Clipboard
                // message, whose body is a flags word and an action rather
                // than latin-1 text.
                let signed = reader.read_i32().await?;
                let len = u64::from(signed.unsigned_abs());
                if !clipboard_enabled {
                    discard(&mut reader, len).await?;
                    continue;
                }
                // Discard an oversized announcement and report its size instead
                // of the first 64 KiB, which would look like the whole thing.
                // The body is consumed either way: the stream position must stay
                // exact whatever the server sends.
                if len > MAX_CLIPBOARD_BYTES as u64 {
                    discard(&mut reader, len).await?;
                    debug!(
                        "vnc: remote clipboard is {len} bytes, over the {MAX_CLIPBOARD_BYTES} byte limit"
                    );
                    let snapshot = {
                        let mut state = clipboard.lock().unwrap();
                        let snapshot = ClipboardSnapshot::oversized(len, state.remote.as_ref());
                        state.remote = Some(snapshot.clone());
                        snapshot
                    };
                    if frame_tx
                        .send(ServerMsg::Clipboard {
                            text: snapshot.text,
                            changed_at_ms: snapshot.changed_at_ms,
                            requested: false,
                            oversized_bytes: snapshot.oversized_bytes,
                        })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    continue;
                }
                let mut bytes = vec![0u8; len as usize];
                reader.read_exact(&mut bytes).await?;

                if signed < 0 {
                    if extended_cut_text(&bytes, &writer, &clipboard, &frame_tx).await? {
                        return Ok(()); // browser link gone
                    }
                    continue;
                }

                let text = latin1_to_string(&bytes);
                debug!("vnc: remote clipboard updated, {} bytes", bytes.len());
                let snapshot = {
                    let mut state = clipboard.lock().unwrap();
                    let snapshot = ClipboardSnapshot::changed(text, state.remote.as_ref());
                    state.remote = Some(snapshot.clone());
                    snapshot
                };
                if frame_tx
                    .send(ServerMsg::Clipboard {
                        text: snapshot.text,
                        changed_at_ms: snapshot.changed_at_ms,
                        requested: false,
                        oversized_bytes: snapshot.oversized_bytes,
                    })
                    .await
                    .is_err()
                {
                    return Ok(()); // browser link gone; the session layer handles it
                }
            }
            other => anyhow::bail!("unknown server message type {other}"),
        }
    }
}

/// Handle one Extended Clipboard message from the server.
///
/// Returns whether the browser link is gone, which is the caller's cue to stop.
/// Everything here is a reply to the server, so it writes rather than returns.
async fn extended_cut_text(
    body: &[u8],
    writer: &SharedWriter,
    clipboard: &SharedClipboard,
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> anyhow::Result<bool> {
    let message = match vnc_clipboard::parse(body) {
        Ok(message) => message,
        Err(e) => {
            // One malformed clipboard message is not worth the session. The
            // stream stayed in sync (the length told us how much to consume),
            // so the next copy can still work.
            warn!("vnc: unreadable extended clipboard message: {e:#}");
            return Ok(false);
        }
    };

    match message {
        // The server's opening move. Record what it can do, then answer with
        // ours — until this arrives the engine assumes latin-1.
        vnc_clipboard::Incoming::Caps(caps) => {
            debug!(
                "vnc: extended clipboard available (actions {:#x}, formats {:#x})",
                caps.actions, caps.formats
            );
            clipboard.lock().unwrap().server = Some(caps);
            write_to(writer, &cut_text_extended(&vnc_clipboard::caps())).await?;
        }
        // The remote copied something. Ask for it, so the browser gets it
        // without anyone pressing Fetch.
        vnc_clipboard::Incoming::Notify(formats) => {
            if formats & vnc_clipboard::FORMAT_TEXT != 0 {
                let request = vnc_clipboard::request(vnc_clipboard::FORMAT_TEXT);
                write_to(writer, &cut_text_extended(&request)).await?;
            } else {
                // An image or file copy, or `formats == 0` for a clipboard
                // that was cleared. Either way the remote no longer holds the
                // text we cached, so drop it — a later Fetch answering with it
                // would be reporting a clipboard that has moved on.
                //
                // Not forwarded as an empty push: the browser would clear an
                // open panel over what may be a screenshot copy. Leaving the
                // panel as it is until something asks is the quieter half of
                // the same truth, and Fetch now answers correctly.
                debug!("vnc: remote copied a format the browser cannot hold");
                let mut state = clipboard.lock().unwrap();
                state.remote = Some(ClipboardSnapshot::changed(
                    String::new(),
                    state.remote.as_ref(),
                ));
            }
        }
        // The answer to that request, or — when there is too much of it to
        // carry — the size it would have been. Both are clipboard activity the
        // panel reports; only one of them has text in it.
        vnc_clipboard::Incoming::Provide(Some(text)) => {
            debug!("vnc: remote clipboard updated, {} bytes (utf-8)", text.len());
            let snapshot = {
                let mut state = clipboard.lock().unwrap();
                let snapshot = ClipboardSnapshot::changed(text, state.remote.as_ref());
                state.remote = Some(snapshot.clone());
                snapshot
            };
            if frame_tx
                .send(ServerMsg::Clipboard {
                    text: snapshot.text,
                    changed_at_ms: snapshot.changed_at_ms,
                    requested: false,
                    oversized_bytes: snapshot.oversized_bytes,
                })
                .await
                .is_err()
            {
                return Ok(true);
            }
        }
        vnc_clipboard::Incoming::Provide(None) => {}
        // Refused, and reported as the size it was: the panel says so instead of
        // showing the first 64 KiB as though it were the whole clipboard.
        vnc_clipboard::Incoming::Oversized(bytes) => {
            debug!(
                "vnc: remote clipboard is {bytes} bytes, over the {MAX_CLIPBOARD_BYTES} byte limit"
            );
            let snapshot = {
                let mut state = clipboard.lock().unwrap();
                let snapshot = ClipboardSnapshot::oversized(bytes, state.remote.as_ref());
                state.remote = Some(snapshot.clone());
                snapshot
            };
            if frame_tx
                .send(ServerMsg::Clipboard {
                    text: snapshot.text,
                    changed_at_ms: snapshot.changed_at_ms,
                    requested: false,
                    oversized_bytes: snapshot.oversized_bytes,
                })
                .await
                .is_err()
            {
                return Ok(true);
            }
        }
        // The server wants what the browser has. This is the deferred half of
        // a browser copy: we advertised with a notify, it asks here.
        vnc_clipboard::Incoming::Request(formats) => {
            let text = clipboard.lock().unwrap().local.clone();
            if let Some(text) = text
                && formats & vnc_clipboard::FORMAT_TEXT != 0
            {
                debug!("vnc: handing {} bytes to the remote's paste", text.len());
                let provide = vnc_clipboard::provide(&text)?;
                write_to(writer, &cut_text_extended(&provide)).await?;
            }
        }
        // "What do you have?" — answered with a notify either way, since
        // silence would leave the server waiting.
        vnc_clipboard::Incoming::Peek => {
            let formats = match clipboard.lock().unwrap().local {
                Some(_) => vnc_clipboard::FORMAT_TEXT,
                None => 0,
            };
            write_to(writer, &cut_text_extended(&vnc_clipboard::notify(formats))).await?;
        }
        vnc_clipboard::Incoming::Unknown(action) => {
            debug!("vnc: ignoring extended clipboard action {action:#x}");
        }
    }
    Ok(false)
}

/// Read one FramebufferUpdate rectangle — raw pixels forwarded as PNG tiles
/// (split into [`STRIP_ROWS`] strips like the RDP engine), or one of the
/// resize pseudo-encodings. Returns whether the desktop was resized.
async fn read_rect(
    reader: &mut Reader,
    writer: &SharedWriter,
    desktop: &SharedDesktop,
    cursor: &SharedCursor,
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> anyhow::Result<bool> {
    let x = reader.read_u16().await?;
    let y = reader.read_u16().await?;
    let w = reader.read_u16().await?;
    let h = reader.read_u16().await?;
    let encoding = reader.read_i32().await?;
    match encoding {
        ENCODING_RAW => {}
        // Cursor: the rect header carries the hotspot (x, y) and the shape
        // size, never a framebuffer position — so it skips the bounds check
        // and tile path below entirely.
        ENCODING_CURSOR => {
            read_cursor(reader, cursor, (x, y, w, h), frame_tx).await?;
            return Ok(false);
        }
        // DesktopSize: the rect itself is the announcement; no payload.
        ENCODING_DESKTOP_SIZE => return apply_resize(desktop, (w, h), frame_tx).await,
        ENCODING_EXTENDED_DESKTOP_SIZE => {
            return read_extended_desktop_size(reader, writer, desktop, (x, y, w, h), frame_tx)
                .await;
        }
        other => anyhow::bail!("server sent encoding {other}, which was not advertised"),
    }

    let size = desktop.lock().unwrap().size;
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
        return Ok(false);
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
    Ok(false)
}

/// Handle a Cursor rect: `w * h` pixels in the negotiated format, followed by
/// a 1-bit-per-pixel transparency mask (rows padded to whole bytes, MSB first,
/// 1 = opaque). The hotspot rides in the rect's x/y. A 0x0 rect means the
/// server hid the pointer.
///
/// Receiving one at all is the server's admission that it is *not* drawing the
/// pointer into the framebuffer, so the shape is cached and forwarded to the
/// browser, which takes over rendering from here.
async fn read_cursor<R: AsyncRead + Unpin>(
    reader: &mut R,
    cursor: &SharedCursor,
    (hx, hy, w, h): (u16, u16, u16, u16),
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    let (state, msg) = if w == 0 || h == 0 {
        debug!("vnc: server hid the pointer");
        (CursorState::Hidden, ServerMsg::Cursor(None))
    } else {
        let pixels_len = usize::from(w) * usize::from(h) * BPP;
        let mask_len = usize::from(w).div_ceil(8) * usize::from(h);
        if w > MAX_CURSOR_DIM || h > MAX_CURSOR_DIM {
            // Drop the shape but not the admission behind it: the server has
            // handed pointer drawing over, so report a hidden pointer and let
            // the browser draw its own arrow instead of nothing at all.
            warn!("vnc: ignoring an oversized {w}x{h} cursor");
            discard(reader, (pixels_len + mask_len) as u64).await?;
            (CursorState::Hidden, ServerMsg::Cursor(None))
        } else {
            let mut pixels = vec![0u8; pixels_len];
            reader.read_exact(&mut pixels).await?;
            let mut mask = vec![0u8; mask_len];
            reader.read_exact(&mut mask).await?;
            let shape =
                CursorShape::from_rgba(w, h, hx, hy, &masked_bgrx_to_rgba(&pixels, &mask, w))?;
            debug!("vnc: cursor {w}x{h} hotspot ({hx},{hy}), {} bytes", shape.png.len());
            (CursorState::Shape(shape.clone()), ServerMsg::Cursor(Some(shape)))
        }
    };
    *cursor.lock().unwrap() = state;
    frame_tx
        .send(msg)
        .await
        .map_err(|_| anyhow::anyhow!("frame channel closed"))
}

/// The [`ServerMsg`] that reproduces the current pointer state for a browser
/// that just attached, or `None` while the server is still drawing it itself.
fn cursor_msg(cursor: &SharedCursor) -> Option<ServerMsg> {
    match &*cursor.lock().unwrap() {
        CursorState::ServerDrawn => None,
        CursorState::Hidden => Some(ServerMsg::Cursor(None)),
        CursorState::Shape(shape) => Some(ServerMsg::Cursor(Some(shape.clone()))),
    }
}

/// Handle an ExtendedDesktopSize rect. The rect header is repurposed by the
/// extension: x = reason (0 server, 1 our SetDesktopSize, 2 another client),
/// y = status when the reason is 1 (0 = ok), w/h = the framebuffer size; the
/// payload is the screen layout. Receiving one at all is the server's
/// declaration that SetDesktopSize is supported.
async fn read_extended_desktop_size<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &Arc<Mutex<W>>,
    desktop: &SharedDesktop,
    (reason, status, w, h): (u16, u16, u16, u16),
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> anyhow::Result<bool> {
    let screens = reader.read_u8().await?;
    let mut padding = [0u8; 3];
    reader.read_exact(&mut padding).await?;
    let mut first = None;
    for i in 0..screens {
        let id = reader.read_u32().await?;
        discard(reader, 8).await?; // x, y, width, height — layout is unused
        let flags = reader.read_u32().await?;
        if i == 0 {
            first = Some(Screen { id, flags });
        }
    }

    let pending = {
        let mut d = desktop.lock().unwrap();
        if first.is_some() {
            d.screen = first;
        }
        d.pending.take()
    };

    let resized = if reason == 1 && status != 0 {
        // Our SetDesktopSize was rejected; the size on the rect is unchanged.
        warn!("vnc: server rejected SetDesktopSize (status {status})");
        false
    } else {
        apply_resize(desktop, (w, h), frame_tx).await?
    };

    // Replay a viewport report that arrived before support was declared.
    if let Some(want) = pending {
        let msg = {
            let d = desktop.lock().unwrap();
            (want != d.size)
                .then(|| d.screen.map(|screen| set_desktop_size(want, screen)))
                .flatten()
        };
        if let Some(msg) = msg {
            debug!("vnc: requesting desktop resize to {}x{} (replayed)", want.0, want.1);
            write_to(writer, &msg).await?;
        }
    }
    Ok(resized)
}

/// Apply a server-announced framebuffer size: update the shared geometry and
/// forward it to the browser. Returns whether the size actually changed.
async fn apply_resize(
    desktop: &SharedDesktop,
    new: (u16, u16),
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        new.0 > 0 && new.1 > 0,
        "server resized the desktop to {}x{}",
        new.0,
        new.1
    );
    {
        let mut d = desktop.lock().unwrap();
        if d.size == new {
            return Ok(false);
        }
        d.size = new;
    }
    info!("vnc: desktop resized to {}x{}", new.0, new.1);
    frame_tx
        .send(ServerMsg::Resize {
            w: new.0,
            h: new.1,
            scale: UNSCALED,
        })
        .await
        .map_err(|_| anyhow::anyhow!("frame channel closed"))?;
    Ok(true)
}

/// Translate one browser input message into RFB client messages, updating the
/// tracked pointer state.
fn translate_input(
    input: ClientMsg,
    button_mask: &mut u8,
    last_pos: &mut (u16, u16),
    pressed_keys: &mut HashMap<String, u32>,
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
        ClientMsg::Key {
            code,
            pressed,
            caps,
        } => {
            // CapsLock is never forwarded: leaving the server's Lock modifier
            // off keeps our pre-resolved keysym from being re-cased by
            // "Shift+Lock" keymap ambiguity. Case is applied here instead, from
            // the browser-reported `caps` state carried on every key event.
            if code == "CapsLock" {
                return Vec::new();
            }
            if pressed {
                // Resolve the symbol against the live modifier state so the
                // shifted keysym (`A`, `!`) is sent, not the base one. CapsLock
                // affects letters only, XORed with Shift.
                let shift_down = pressed_keys.contains_key("ShiftLeft")
                    || pressed_keys.contains_key("ShiftRight");
                let is_letter = matches!(code.as_bytes(), [b'K', b'e', b'y', b'A'..=b'Z']);
                let shift = if is_letter { shift_down ^ caps } else { shift_down };
                match keymap::keysym(&code, shift) {
                    Some(sym) => {
                        pressed_keys.insert(code, sym);
                        key_event(true, sym).to_vec()
                    }
                    None => {
                        debug!("vnc: unmapped key code {code}");
                        Vec::new()
                    }
                }
            } else {
                // Release exactly what was pressed; fall back to the unshifted
                // keysym for a release with no matching press.
                match pressed_keys
                    .remove(&code)
                    .or_else(|| keymap::keysym(&code, false))
                {
                    Some(sym) => key_event(false, sym).to_vec(),
                    None => {
                        debug!("vnc: unmapped key code {code}");
                        Vec::new()
                    }
                }
            }
        }
        // Intercepted by the input loop (request_resize) before translation.
        ClientMsg::Viewport { .. } => Vec::new(),
        // Intercepted by the input loop (full repaint) before translation.
        ClientMsg::Refresh => Vec::new(),
        // Intercepted by the input loop (the clipboard bridge, which needs the
        // shared buffer and frame_tx) before translation.
        ClientMsg::Clipboard { .. } | ClientMsg::ClipboardRequest => Vec::new(),
        // Session-control messages act on the slot, not an engine — the ws
        // bridge handles them and they never reach here.
        ClientMsg::Connect { .. } | ClientMsg::Disconnect => Vec::new(),
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

/// SetDesktopSize: ask the server to re-render at the given framebuffer size,
/// laid out as a single screen echoing the server's screen id and flags.
fn set_desktop_size(size: (u16, u16), screen: Screen) -> [u8; 24] {
    let mut msg = [0u8; 24];
    msg[0] = 251; // message type
    // msg[1]: padding
    msg[2..4].copy_from_slice(&size.0.to_be_bytes());
    msg[4..6].copy_from_slice(&size.1.to_be_bytes());
    msg[6] = 1; // number of screens
    // msg[7]: padding
    msg[8..12].copy_from_slice(&screen.id.to_be_bytes());
    // msg[12..16]: screen x, y = 0
    msg[16..18].copy_from_slice(&size.0.to_be_bytes());
    msg[18..20].copy_from_slice(&size.1.to_be_bytes());
    msg[20..24].copy_from_slice(&screen.flags.to_be_bytes());
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

/// ClientCutText: put `text` on the remote's clipboard.
///
/// RFB cut text is latin-1 ([`latin1_from_str`]). `None` over
/// [`MAX_CLIPBOARD_BYTES`]: the caller has already refused by then, and an
/// encoder that quietly truncated instead would be the one place a partial
/// paste could still reach a remote.
fn client_cut_text(text: &str) -> Option<Vec<u8>> {
    if !clipboard_fits(text) {
        return None;
    }
    let bytes = latin1_from_str(text);
    let mut msg = vec![6u8, 0, 0, 0]; // message type + 3 padding
    msg.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    msg.extend_from_slice(&bytes);
    Some(msg)
}

/// ClientCutText carrying an Extended Clipboard body.
///
/// Same message type as [`client_cut_text`]; the negative length is the whole
/// signal that the payload is a flags word rather than latin-1 text.
fn cut_text_extended(body: &[u8]) -> Vec<u8> {
    let mut msg = vec![6u8, 0, 0, 0]; // message type + 3 padding
    msg.extend_from_slice(&(-(body.len() as i32)).to_be_bytes());
    msg.extend_from_slice(body);
    msg
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Decode RFB cut text (latin-1) into a `String`: every byte is the codepoint
/// of the same value.
fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Encode a `String` as RFB cut text (latin-1).
///
/// Anything outside latin-1 becomes `?`, which is what noVNC does and all the
/// baseline protocol can carry — RFB's UTF-8 clipboard lives in the Extended
/// Clipboard pseudo-encoding, which this client does not negotiate.
///
/// Length is [`client_cut_text`]'s business: latin-1 spends one byte per char
/// where UTF-8 spends at least one, so text that fits the ceiling as UTF-8
/// cannot exceed it here.
fn latin1_from_str(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

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

/// Whether the far end is macOS Screen Sharing, from what it said during the
/// handshake. Apple's server announces its own protocol revision, RFB 003.889,
/// and offers Apple's security types (30 = ARD, 35 = Mac authentication)
/// alongside the standard ones — no other server does either.
///
/// A third-party VNC server running on a Mac looks like any other server here
/// and is reported as not-macOS. What that costs is the native viewer's
/// keyboard convention, not correctness, which is why guessing from a desktop
/// name is not worth it.
fn is_macos_server(minor: u32, security_types: &[u8]) -> bool {
    minor == 889 || security_types.iter().any(|t| matches!(t, 30 | 35))
}

/// Repack BGRX pixels (our forced format on the wire) into packed RGB888.
fn bgrx_to_rgb(bgrx: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(bgrx.len() / BPP * 3);
    for px in bgrx.chunks_exact(BPP) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    rgb
}

/// Repack a cursor's BGRX pixels into RGBA, folding the RFB 1-bit mask into
/// the alpha channel: rows are padded to whole bytes and scanned MSB first,
/// with a set bit meaning opaque. Pixels outside the mask are cleared to fully
/// transparent black rather than just alpha-zeroed, so PNG's filtering has a
/// flat area to compress and no stale colour can bleed through a viewer that
/// ignores alpha.
fn masked_bgrx_to_rgba(bgrx: &[u8], mask: &[u8], w: u16) -> Vec<u8> {
    let stride = usize::from(w).div_ceil(8);
    let mut rgba = Vec::with_capacity(bgrx.len());
    for (i, px) in bgrx.chunks_exact(BPP).enumerate() {
        let (row, col) = (i / usize::from(w), i % usize::from(w));
        let opaque = mask
            .get(row * stride + col / 8)
            .is_some_and(|byte| byte >> (7 - col % 8) & 1 == 1);
        if opaque {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        } else {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        }
    }
    rgba
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
async fn discard<R: AsyncRead + Unpin>(reader: &mut R, n: u64) -> anyhow::Result<()> {
    let copied = tokio::io::copy(&mut reader.take(n), &mut tokio::io::sink()).await?;
    anyhow::ensure!(copied == n, "connection closed while skipping {n} bytes");
    Ok(())
}

async fn write_to<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<W>>,
    bytes: &[u8],
) -> anyhow::Result<()> {
    writer
        .lock()
        .await
        .write_all(bytes)
        .await
        .map_err(|e| anyhow::anyhow!("write to VNC server: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors generated from a reference VNC auth implementation
    // (node:crypto des-ecb) with the challenge 00 01 .. 0f.
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
    fn a_mac_is_recognized_from_its_handshake() {
        // macOS Screen Sharing, exactly as macOS 26 answered on the test VM:
        // Apple's revision, and Apple's security types around the standard
        // one. Either signal alone is enough.
        assert!(is_macos_server(889, &[30, 33, 36, 2, 35]));
        assert!(is_macos_server(889, &[2]));
        assert!(is_macos_server(8, &[30, 2]));
        assert!(is_macos_server(8, &[35]));

        // Everyone else — the first line is what the test Linux box answered.
        assert!(!is_macos_server(8, &[2]));
        assert!(!is_macos_server(8, &[1, 2, 16, 18]));
        assert!(!is_macos_server(1, &[1]));
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
    fn client_cut_text_is_type_6_with_a_big_endian_length() {
        let msg = client_cut_text("hi").expect("fits");
        assert_eq!(msg[0], 6);
        assert_eq!(&msg[1..4], &[0, 0, 0]); // padding
        assert_eq!(&msg[4..8], &2u32.to_be_bytes()); // length, big-endian
        assert_eq!(&msg[8..], b"hi");

        // Empty text is a well-formed message, not a skipped one — clearing the
        // remote clipboard is a legitimate thing to ask for.
        let msg = client_cut_text("").expect("fits");
        assert_eq!(msg.len(), 8);
        assert_eq!(&msg[4..8], &0u32.to_be_bytes());
    }

    #[test]
    fn cut_text_is_latin1_with_a_question_mark_for_the_rest() {
        // Latin-1 survives; anything above U+00FF degrades to '?'.
        let msg = client_cut_text("café ☕").expect("fits");
        assert_eq!(&msg[8..], &[b'c', b'a', b'f', 0xE9, b' ', b'?']);

        // Round trip: what a server echoes back decodes to the same latin-1.
        assert_eq!(latin1_to_string(&msg[8..]), "café ?");
        // Every byte maps to the codepoint of the same value, 0x80..0x9F
        // included (latin-1, not Windows-1252).
        assert_eq!(latin1_to_string(&[0x00, 0x80, 0xFF]), "\u{0}\u{80}\u{ff}");
    }

    // Refused, not truncated: this encoder is the last place a partial paste
    // could still reach a remote, and one byte of latin-1 per char means it
    // cannot silently overshoot either.
    #[test]
    fn cut_text_over_the_ceiling_is_refused() {
        assert_eq!(client_cut_text(&"a".repeat(MAX_CLIPBOARD_BYTES + 1)), None);
        // Measured in UTF-8 bytes, so multi-byte characters hit it sooner than
        // their latin-1 '?' would suggest.
        assert_eq!(client_cut_text(&"☕".repeat(MAX_CLIPBOARD_BYTES)), None);

        // At the ceiling it still encodes, so the boundary is inclusive.
        let msg = client_cut_text(&"a".repeat(MAX_CLIPBOARD_BYTES)).expect("fits");
        assert_eq!(msg.len(), 8 + MAX_CLIPBOARD_BYTES);
        assert_eq!(&msg[4..8], &(MAX_CLIPBOARD_BYTES as u32).to_be_bytes());
    }

    #[test]
    fn raw_only_encoding_set() {
        assert_eq!(set_encodings(&[ENCODING_RAW]), vec![2, 0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn resize_encoding_set_appends_the_pseudo_encodings() {
        let msg = set_encodings(&[
            ENCODING_RAW,
            ENCODING_CURSOR,
            ENCODING_EXTENDED_DESKTOP_SIZE,
            ENCODING_DESKTOP_SIZE,
        ]);
        assert_eq!(&msg[..4], &[2, 0, 0, 4]);
        assert_eq!(&msg[4..8], &0i32.to_be_bytes());
        assert_eq!(&msg[8..12], &(-239i32).to_be_bytes());
        assert_eq!(&msg[12..16], &(-308i32).to_be_bytes());
        assert_eq!(&msg[16..20], &(-223i32).to_be_bytes());
    }

    // ── Cursor pseudo-encoding ──────────────────────────────────────────────

    #[test]
    fn cursor_mask_becomes_alpha_and_masked_out_pixels_are_cleared() {
        // 3x2 cursor: mask rows are padded to a whole byte, MSB first.
        // Row 0: 101xxxxx, row 1: 010xxxxx.
        let bgrx: Vec<u8> = (0..6).flat_map(|i| [i * 3, i * 3 + 1, i * 3 + 2, 0]).collect();
        let rgba = masked_bgrx_to_rgba(&bgrx, &[0b1010_0000, 0b0100_0000], 3);
        assert_eq!(
            rgba,
            vec![
                2, 1, 0, 255, // (0,0) opaque, BGRX -> RGBA
                0, 0, 0, 0, // (1,0) transparent
                8, 7, 6, 255, // (2,0) opaque
                0, 0, 0, 0, // (0,1) transparent
                14, 13, 12, 255, // (1,1) opaque
                0, 0, 0, 0, // (2,1) transparent
            ]
        );
    }

    /// Decode a cursor's PNG back to RGBA for assertions.
    fn decode_rgba(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba);
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[tokio::test]
    async fn cursor_rect_is_cached_and_forwarded_as_an_rgba_png() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (tx, mut rx) = mpsc::channel(4);
        // 2x1: an opaque red pixel then a masked-out one.
        let mut payload = vec![0, 0, 255, 0, 9, 9, 9, 0]; // BGRX
        payload.push(0b1000_0000); // mask row
        let mut reader = payload.as_slice();

        read_cursor(&mut reader, &cursor, (1, 2, 2, 1), &tx).await.unwrap();

        let shape = match rx.try_recv().unwrap() {
            ServerMsg::Cursor(Some(shape)) => shape,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!((shape.w, shape.h, shape.hx, shape.hy), (2, 1, 1, 2));
        assert_eq!(decode_rgba(&shape.png), (2, 1, vec![255, 0, 0, 255, 0, 0, 0, 0]));
        // Cached for replay to a browser that attaches later.
        match cursor_msg(&cursor) {
            Some(ServerMsg::Cursor(Some(cached))) => assert_eq!(cached, shape),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_cursor_rect_hides_the_pointer() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (tx, mut rx) = mpsc::channel(4);
        // No payload at all for a 0x0 rect.
        read_cursor(&mut [].as_slice(), &cursor, (0, 0, 0, 0), &tx).await.unwrap();
        assert!(matches!(rx.try_recv(), Ok(ServerMsg::Cursor(None))));
        // Hidden is still browser-drawn state, so it replays on reattach —
        // unlike ServerDrawn, which must stay silent.
        assert!(matches!(cursor_msg(&cursor), Some(ServerMsg::Cursor(None))));
    }

    #[tokio::test]
    async fn oversized_cursor_is_drained_and_hides_the_pointer() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        let (tx, mut rx) = mpsc::channel(4);
        let (w, h) = (MAX_CURSOR_DIM + 1, 1);
        let mut payload = vec![0u8; usize::from(w) * BPP + usize::from(w).div_ceil(8)];
        // A trailing byte stands in for the next rect: it must survive.
        payload.push(0xAB);
        let mut reader = payload.as_slice();

        read_cursor(&mut reader, &cursor, (0, 0, w, h), &tx).await.unwrap();
        assert_eq!(reader, &[0xAB]);
        // The shape is dropped, but the server still isn't drawing the pointer,
        // so the browser is told to fall back rather than left with nothing.
        assert!(matches!(rx.try_recv(), Ok(ServerMsg::Cursor(None))));
        assert!(matches!(cursor_msg(&cursor), Some(ServerMsg::Cursor(None))));
    }

    #[test]
    fn no_cursor_rect_leaves_pointer_rendering_to_the_server() {
        let cursor: SharedCursor = Arc::new(std::sync::Mutex::new(CursorState::default()));
        assert!(cursor_msg(&cursor).is_none());
    }

    #[test]
    fn set_desktop_size_encodes_a_single_screen() {
        let msg = set_desktop_size((1920, 1200), Screen { id: 0x0A0B0C0D, flags: 1 });
        assert_eq!(msg[0], 251); // message type
        assert_eq!(msg[1], 0); // padding
        assert_eq!(&msg[2..6], &[0x07, 0x80, 0x04, 0xB0]); // 1920, 1200
        assert_eq!((msg[6], msg[7]), (1, 0)); // one screen + padding
        assert_eq!(&msg[8..12], &[0x0A, 0x0B, 0x0C, 0x0D]); // screen id
        assert_eq!(&msg[12..16], &[0; 4]); // screen x, y = 0
        assert_eq!(&msg[16..20], &[0x07, 0x80, 0x04, 0xB0]); // screen w, h
        assert_eq!(&msg[20..24], &[0, 0, 0, 1]); // flags echoed
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
        let mut keys = HashMap::new();

        let bytes = translate_input(
            ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            &mut mask,
            &mut pos,
            &mut keys,
        );
        assert_eq!(bytes, pointer_event(0x01, (10, 20)).to_vec());

        // A move while the button is held keeps it in the mask (drag).
        let bytes = translate_input(
            ClientMsg::MouseMove { x: 30, y: 40 },
            &mut mask,
            &mut pos,
            &mut keys,
        );
        assert_eq!(bytes, pointer_event(0x01, (30, 40)).to_vec());

        // Scroll down = button 5 (0x10) press + release, on top of the held mask.
        let bytes = translate_input(
            ClientMsg::Wheel { dx: 0.0, dy: 3.0 },
            &mut mask,
            &mut pos,
            &mut keys,
        );
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
            &mut keys,
        );
        assert_eq!(bytes, pointer_event(0x00, (30, 40)).to_vec());
    }

    // ── Resize state machine (no sockets: Cursor writer, slice reader) ──────

    type TestWriter = Arc<Mutex<std::io::Cursor<Vec<u8>>>>;

    fn test_writer() -> TestWriter {
        Arc::new(Mutex::new(std::io::Cursor::new(Vec::new())))
    }

    async fn written(writer: &TestWriter) -> Vec<u8> {
        writer.lock().await.get_ref().clone()
    }

    fn shared_desktop(
        size: (u16, u16),
        screen: Option<Screen>,
        pending: Option<(u16, u16)>,
    ) -> SharedDesktop {
        Arc::new(std::sync::Mutex::new(DesktopState { size, screen, pending }))
    }

    /// Payload of an ExtendedDesktopSize rect declaring one screen.
    fn eds_payload(screen: Screen) -> Vec<u8> {
        let mut p = vec![1, 0, 0, 0]; // one screen + padding
        p.extend_from_slice(&screen.id.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]); // screen x, y, w, h (layout unused)
        p.extend_from_slice(&screen.flags.to_be_bytes());
        p
    }

    #[tokio::test]
    async fn request_resize_stashes_until_support_and_skips_noops() {
        let writer = test_writer();
        let desktop = shared_desktop((1024, 768), None, None);

        // Matching the current size or a zero dimension: no-ops.
        request_resize(&writer, &desktop, (1024, 768)).await.unwrap();
        request_resize(&writer, &desktop, (0, 600)).await.unwrap();
        assert!(desktop.lock().unwrap().pending.is_none());
        assert!(written(&writer).await.is_empty());

        // Support not declared yet: stashed, nothing on the wire.
        request_resize(&writer, &desktop, (800, 600)).await.unwrap();
        assert_eq!(desktop.lock().unwrap().pending, Some((800, 600)));
        assert!(written(&writer).await.is_empty());

        // Browser back at the current size: the stale stash is dropped.
        request_resize(&writer, &desktop, (1024, 768)).await.unwrap();
        assert!(desktop.lock().unwrap().pending.is_none());

        // Support declared: SetDesktopSize goes out immediately.
        let screen = Screen { id: 7, flags: 0 };
        desktop.lock().unwrap().screen = Some(screen);
        request_resize(&writer, &desktop, (800, 600)).await.unwrap();
        assert_eq!(written(&writer).await, set_desktop_size((800, 600), screen));
    }

    #[tokio::test]
    async fn extended_desktop_size_declares_support_and_replays_pending() {
        let writer = test_writer();
        let (tx, mut rx) = mpsc::channel(8);
        let desktop = shared_desktop((1024, 768), None, Some((800, 600)));
        let screen = Screen { id: 3, flags: 0 };

        // First rect from the server (reason 0), size unchanged.
        let payload = eds_payload(screen);
        let resized = read_extended_desktop_size(
            &mut payload.as_slice(),
            &writer,
            &desktop,
            (0, 0, 1024, 768),
            &tx,
        )
        .await
        .unwrap();

        assert!(!resized, "size did not change");
        let (screen_id, pending) = {
            let d = desktop.lock().unwrap();
            (d.screen.map(|s| s.id), d.pending)
        };
        assert_eq!(screen_id, Some(3), "support recorded");
        assert_eq!(pending, None, "stash consumed");
        // No browser resize (same size), but the stashed report replays.
        assert!(rx.try_recv().is_err());
        assert_eq!(written(&writer).await, set_desktop_size((800, 600), screen));
    }

    #[tokio::test]
    async fn extended_desktop_size_applies_a_change_and_tells_the_browser() {
        let writer = test_writer();
        let (tx, mut rx) = mpsc::channel(8);
        let desktop = shared_desktop((1024, 768), None, None);

        // Our SetDesktopSize succeeded (reason 1, status 0) at 800x600.
        let payload = eds_payload(Screen { id: 1, flags: 0 });
        let resized = read_extended_desktop_size(
            &mut payload.as_slice(),
            &writer,
            &desktop,
            (1, 0, 800, 600),
            &tx,
        )
        .await
        .unwrap();

        assert!(resized);
        assert_eq!(desktop.lock().unwrap().size, (800, 600));
        assert!(matches!(rx.try_recv(), Ok(ServerMsg::Resize { w: 800, h: 600, scale: UNSCALED })));
        assert!(written(&writer).await.is_empty(), "nothing left to request");
    }

    #[tokio::test]
    async fn rejected_set_desktop_size_leaves_the_size_alone() {
        let writer = test_writer();
        let (tx, mut rx) = mpsc::channel(8);
        let desktop = shared_desktop((1024, 768), Some(Screen { id: 1, flags: 0 }), None);

        // reason 1, status 1 = our request was prohibited.
        let payload = eds_payload(Screen { id: 1, flags: 0 });
        let resized = read_extended_desktop_size(
            &mut payload.as_slice(),
            &writer,
            &desktop,
            (1, 1, 640, 480),
            &tx,
        )
        .await
        .unwrap();

        assert!(!resized);
        assert_eq!(desktop.lock().unwrap().size, (1024, 768));
        assert!(rx.try_recv().is_err(), "no resize reported to the browser");
        assert!(written(&writer).await.is_empty());
    }

    #[tokio::test]
    async fn apply_resize_dedupes_and_rejects_zero_sizes() {
        let (tx, mut rx) = mpsc::channel(8);
        let desktop = shared_desktop((1024, 768), None, None);

        // Same size: no change, nothing sent to the browser.
        assert!(!apply_resize(&desktop, (1024, 768), &tx).await.unwrap());
        assert!(rx.try_recv().is_err());

        // A real change updates the state and reaches the browser.
        assert!(apply_resize(&desktop, (640, 480), &tx).await.unwrap());
        assert_eq!(desktop.lock().unwrap().size, (640, 480));
        assert!(matches!(rx.try_recv(), Ok(ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED })));

        // A zero dimension is a protocol violation, not a resize.
        assert!(apply_resize(&desktop, (0, 480), &tx).await.is_err());
    }

    /// Feed one key event through `translate_input`, carrying the browser's
    /// `caps` state (as the wire message does) and sharing the pressed-key map.
    fn key(keys: &mut HashMap<String, u32>, code: &str, pressed: bool, caps: bool) -> Vec<u8> {
        let (mut mask, mut pos) = (0u8, (0u16, 0u16));
        translate_input(
            ClientMsg::Key {
                code: code.to_owned(),
                pressed,
                caps,
            },
            &mut mask,
            &mut pos,
            keys,
        )
    }

    #[test]
    fn key_input_maps_to_keysyms_and_drops_unknown_codes() {
        let mut keys = HashMap::new();
        assert_eq!(
            key(&mut keys, "KeyA", true, false),
            key_event(true, 0x61).to_vec()
        );
        assert!(key(&mut keys, "MediaPlayPause", true, false).is_empty());
    }

    #[test]
    fn held_shift_sends_the_shifted_keysym() {
        let mut keys = HashMap::new();
        // Shift down, then a letter and a digit resolve to their shifted form.
        assert_eq!(
            key(&mut keys, "ShiftLeft", true, false),
            key_event(true, 0xFFE1).to_vec()
        );
        assert_eq!(
            key(&mut keys, "KeyA", true, false),
            key_event(true, 0x41).to_vec()
        ); // 'A'
        assert_eq!(
            key(&mut keys, "Digit1", true, false),
            key_event(true, 0x21).to_vec()
        ); // '!'
    }

    #[test]
    fn release_uses_the_keysym_from_press_even_after_shift_is_let_go() {
        let mut keys = HashMap::new();
        key(&mut keys, "ShiftLeft", true, false);
        assert_eq!(
            key(&mut keys, "KeyA", true, false),
            key_event(true, 0x41).to_vec()
        ); // 'A' down
        // Shift released before the letter — the letter must still release 'A',
        // not 'a', or the server leaves the shifted keysym stuck down.
        key(&mut keys, "ShiftLeft", false, false);
        assert_eq!(
            key(&mut keys, "KeyA", false, false),
            key_event(false, 0x41).to_vec()
        );
        assert!(keys.is_empty());
    }

    #[test]
    fn capslock_key_is_never_forwarded() {
        let mut keys = HashMap::new();
        // The CapsLock key itself produces no wire bytes and holds no state.
        assert!(key(&mut keys, "CapsLock", true, true).is_empty());
        assert!(key(&mut keys, "CapsLock", false, true).is_empty());
        assert!(keys.is_empty());
    }

    #[test]
    fn caps_flag_uppercases_letters_only() {
        let mut keys = HashMap::new();
        // With the browser reporting CapsLock on, a plain letter is uppercased.
        assert_eq!(
            key(&mut keys, "KeyA", true, true),
            key_event(true, 0x41).to_vec()
        ); // 'A'
        key(&mut keys, "KeyA", false, true);
        // Digits/symbols are unaffected by CapsLock.
        assert_eq!(
            key(&mut keys, "Digit1", true, true),
            key_event(true, u32::from('1')).to_vec()
        );
    }

    #[test]
    fn caps_and_shift_cancel_for_letters() {
        let mut keys = HashMap::new();
        key(&mut keys, "ShiftLeft", true, true); // shift held, caps on
        // caps XOR shift = off → lowercase letter.
        assert_eq!(
            key(&mut keys, "KeyA", true, true),
            key_event(true, 0x61).to_vec()
        ); // 'a'
    }
}
