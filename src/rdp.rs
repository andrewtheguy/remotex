//! Server-side RDP session driven by [IronRDP](https://crates.io/crates/ironrdp).
//!
//! The web server never speaks RDP to the browser: [`crate::ws`] bridges a
//! browser WebSocket to [`run`] here over a pair of channels. `run` connects to
//! the configured RDP host (TCP → TLS → RDP activation), then drives the active
//! session — decoding the framebuffer into [`ServerMsg::Tile`] updates and
//! injecting [`ClientMsg`] input as RDP fast-path PDUs.
//!
//! See docs/architecture.md for the design.

use ironrdp::connector::{
    ClientConnector, ConnectionResult, Config, Credentials, DesktopSize, ServerName,
};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::input::MousePdu;
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::input::mouse::PointerFlags;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use ironrdp_tokio::{FramedWrite as _, TokioFramed, split_tokio_framed};
use log::{debug, info, warn};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::config::TargetConfig;
use crate::keymap;
use crate::protocol::{ClientMsg, MouseButton, STRIP_ROWS, ServerMsg, Tile};

// A type-erased async stream, so the connect path (which upgrades TCP → TLS) can
// return a single concrete framed type.
trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite {}
type UpgradedFramed = TokioFramed<Box<dyn AsyncReadWrite + Unpin + Send + Sync>>;

/// Connect to the RDP host, then drive the session until it ends.
///
/// `input_rx` carries browser input; `frame_tx` carries screen updates back.
/// Both closing (browser gone / RDP ended) tears the session down.
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) {
    let (connection_result, framed) = match connect(&config).await {
        Ok(v) => v,
        Err(e) => {
            warn!("rdp: connect failed: {e:#}");
            let _ = frame_tx
                .send(ServerMsg::Error {
                    message: format!("RDP connect failed: {e}"),
                })
                .await;
            return;
        }
    };

    let desktop = connection_result.desktop_size;
    info!("rdp: connected, desktop {}x{}", desktop.width, desktop.height);
    if frame_tx
        .send(ServerMsg::Resize {
            w: desktop.width,
            h: desktop.height,
        })
        .await
        .is_err()
    {
        return; // browser already gone
    }

    if let Err(e) = active_loop(connection_result, framed, input_rx, frame_tx.clone()).await {
        warn!("rdp: session error: {e:#}");
        let _ = frame_tx
            .send(ServerMsg::Error {
                message: format!("RDP session ended: {e}"),
            })
            .await;
    }
    info!("rdp: session terminated");
}

/// TCP connect → RDP negotiation → TLS upgrade → CredSSP/finalize.
async fn connect(config: &TargetConfig) -> anyhow::Result<(ConnectionResult, UpgradedFramed)> {
    let server_name = config.host.clone();
    let dest = host_port(&config.host, config.port);

    let stream = TcpStream::connect(&dest)
        .await
        .map_err(|e| anyhow::anyhow!("TCP connect to {dest}: {e}"))?;
    stream.set_nodelay(true).ok();
    let client_addr = stream
        .local_addr()
        .map_err(|e| anyhow::anyhow!("get local address: {e}"))?;

    let mut framed = TokioFramed::new(stream);
    let mut connector = ClientConnector::new(build_connector_config(config), client_addr);

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .map_err(|e| anyhow::anyhow!("RDP negotiation (connect_begin): {}", describe(&e)))?;

    let (initial_stream, leftover) = framed.into_inner();

    let (tls_stream, tls_cert) = ironrdp_tls::upgrade(initial_stream, &server_name)
        .await
        .map_err(|e| anyhow::anyhow!("TLS upgrade: {e}"))?;

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);

    let erased: Box<dyn AsyncReadWrite + Unpin + Send + Sync> = Box::new(tls_stream);
    let mut upgraded_framed = TokioFramed::new_with_leftover(erased, leftover);

    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&tls_cert)
        .ok_or_else(|| anyhow::anyhow!("could not extract TLS server public key"))?
        .to_owned();

    let connection_result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut ReqwestNetworkClient::new(),
        ServerName::new(&server_name),
        server_public_key,
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("RDP activation (connect_finalize): {}", describe(&e)))?;

    Ok((connection_result, upgraded_framed))
}

/// Drive the active RDP session: server frames in, input out, tiles back.
async fn active_loop(
    connection_result: ConnectionResult,
    framed: UpgradedFramed,
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = split_tokio_framed(framed);

    let desktop = connection_result.desktop_size;
    let mut image = DecodedImage::new(PixelFormat::RgbA32, desktop.width, desktop.height);

    let mut active_stage = ActiveStageBuilder {
        static_channels: connection_result.static_channels,
        user_channel_id: connection_result.user_channel_id,
        io_channel_id: connection_result.io_channel_id,
        message_channel_id: connection_result.message_channel_id,
        share_id: connection_result.share_id,
        compression_type: connection_result.compression_type,
        enable_server_pointer: connection_result.enable_server_pointer,
        pointer_software_rendering: connection_result.pointer_software_rendering,
    }
    .build();

    // Last known pointer position, so button/wheel events (which the browser
    // sends without coordinates) land where the cursor actually is.
    let mut last_pos: (u16, u16) = (desktop.width / 2, desktop.height / 2);

    loop {
        let outputs = tokio::select! {
            frame = reader.read_pdu() => {
                let (action, payload) = frame.map_err(|e| anyhow::anyhow!("read frame: {e}"))?;
                active_stage
                    .process(&mut image, action, &payload)
                    .map_err(|e| anyhow::anyhow!("process frame: {e}"))?
            }
            input = input_rx.recv() => {
                let Some(input) = input else {
                    info!("rdp: input channel closed by browser");
                    break;
                };
                let events = translate_input(input, &mut last_pos);
                if events.is_empty() {
                    continue;
                }
                active_stage
                    .process_fastpath_input(&mut image, &events)
                    .map_err(|e| anyhow::anyhow!("process input: {e}"))?
            }
        };

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => {
                    writer
                        .write_all(&frame)
                        .await
                        .map_err(|e| anyhow::anyhow!("write response: {e}"))?;
                }
                ActiveStageOutput::GraphicsUpdate(region) => {
                    send_tiles(
                        &image,
                        region.left,
                        region.top,
                        region.right,
                        region.bottom,
                        &frame_tx,
                    )
                    .await?;
                }
                ActiveStageOutput::Terminate(reason) => {
                    info!("rdp: session terminated by server: {reason:?}");
                    return Ok(());
                }
                ActiveStageOutput::DeactivateAll => {
                    // Deactivation-Reactivation (e.g. resolution change) is out of
                    // scope — see docs/architecture.md.
                    warn!("rdp: received DeactivateAll (reactivation not implemented)");
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Translate one browser input message into RDP fast-path input events.
fn translate_input(input: ClientMsg, last_pos: &mut (u16, u16)) -> Vec<FastPathInputEvent> {
    match input {
        ClientMsg::MouseMove { x, y } => {
            let (x, y) = (clamp_u16(x), clamp_u16(y));
            *last_pos = (x, y);
            vec![FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::MOVE,
                number_of_wheel_rotation_units: 0,
                x_position: x,
                y_position: y,
            })]
        }
        ClientMsg::MouseButton { button, pressed } => {
            let mut flags = match button {
                MouseButton::Left => PointerFlags::LEFT_BUTTON,
                MouseButton::Right => PointerFlags::RIGHT_BUTTON,
                MouseButton::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
            };
            if pressed {
                flags |= PointerFlags::DOWN;
            }
            vec![FastPathInputEvent::MouseEvent(MousePdu {
                flags,
                number_of_wheel_rotation_units: 0,
                x_position: last_pos.0,
                y_position: last_pos.1,
            })]
        }
        ClientMsg::Wheel { dx, dy } => {
            let mut events = Vec::new();
            // RDP: positive rotation is up/forward. The DOM deltaY is positive
            // when scrolling down, so invert it. One notch ≈ 120 units.
            if dy != 0.0 {
                events.push(FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::VERTICAL_WHEEL,
                    number_of_wheel_rotation_units: if dy > 0.0 { -120 } else { 120 },
                    x_position: last_pos.0,
                    y_position: last_pos.1,
                }));
            }
            if dx != 0.0 {
                events.push(FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::HORIZONTAL_WHEEL,
                    number_of_wheel_rotation_units: if dx > 0.0 { 120 } else { -120 },
                    x_position: last_pos.0,
                    y_position: last_pos.1,
                }));
            }
            events
        }
        ClientMsg::Key { code, pressed } => match keymap::scancode(&code) {
            Some((scancode, extended)) => {
                let mut flags = KeyboardFlags::empty();
                if !pressed {
                    flags |= KeyboardFlags::RELEASE;
                }
                if extended {
                    flags |= KeyboardFlags::EXTENDED;
                }
                vec![FastPathInputEvent::KeyboardEvent(flags, scancode)]
            }
            None => {
                debug!("rdp: unmapped key code {code}");
                Vec::new()
            }
        },
        // RDP can't resize mid-session without Deactivation-Reactivation,
        // which is not implemented; the frontend keeps its scrollbars.
        ClientMsg::Viewport { .. } => Vec::new(),
    }
}

/// Repack the dirty rectangle `[left..=right] × [top..=bottom]` into packed
/// RGB strips and send each as a [`ServerMsg::Tile`] (binary WS frame with a
/// PNG-compressed payload — see `protocol::Tile`).
async fn send_tiles(
    image: &DecodedImage,
    left: u16,
    top: u16,
    right: u16,
    bottom: u16,
    frame_tx: &mpsc::Sender<ServerMsg>,
) -> anyhow::Result<()> {
    if right < left || bottom < top {
        return Ok(());
    }
    let width = right - left + 1;
    let total_h = bottom - top + 1;
    let bpp = image.bytes_per_pixel();
    let stride = image.stride();
    let data = image.data();

    let mut done = 0u16;
    while done < total_h {
        let h = STRIP_ROWS.min(total_h - done);
        let y0 = top + done;

        // Pack to RGB888: the framebuffer alpha is meaningless for a screen
        // (and IronRDP may leave it 0), so it is dropped rather than shipped.
        let mut buf = Vec::with_capacity(usize::from(width) * usize::from(h) * 3);
        for r in 0..h {
            let src_y = usize::from(y0 + r);
            let start = src_y * stride + usize::from(left) * bpp;
            let line = &data[start..start + usize::from(width) * bpp];
            for px in line.chunks_exact(bpp) {
                buf.extend_from_slice(&px[..3]);
            }
        }

        let tile = Tile::from_rgb(left, y0, width, h, &buf)?;
        debug!(
            "rdp: tile {width}x{h} at ({left},{y0}): {} -> {} bytes",
            buf.len(),
            tile.data.len()
        );
        frame_tx
            .send(ServerMsg::Tile(tile))
            .await
            .map_err(|_| anyhow::anyhow!("frame channel closed"))?;

        done += h;
    }

    Ok(())
}

fn clamp_u16(v: i32) -> u16 {
    v.clamp(0, i32::from(u16::MAX)) as u16
}

/// Render an error together with its full `source()` chain, so wrappers like
/// IronRDP's `ConnectorError` reveal the underlying cause (e.g. the CredSSP /
/// SSPI reason) instead of just a top-level label.
fn describe(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(" -> ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

/// Format a `host:port` destination for `TcpStream::connect`, bracketing bare
/// IPv6 literals (e.g. `fdb8::20` -> `[fdb8::20]:3389`).
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Build the IronRDP connector config from our runtime config.
///
/// Enables both TLS and CredSSP/NLA so the server can negotiate the strongest
/// security it supports. Modeled on the IronRDP `screenshot` example.
fn build_connector_config(config: &TargetConfig) -> Config {
    let (enable_tls, enable_credssp) = config.security.flags();
    Config {
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        enable_tls,
        enable_credssp,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize {
            width: config.width,
            height: config.height,
        },
        bitmap: None,
        client_build: 0,
        client_name: "rdpweb".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),

        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(target_os = "linux")]
        platform: MajorPlatformType::UNIX,
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        platform: MajorPlatformType::UNIX,

        // Render the server pointer into the framebuffer so the cursor is visible.
        enable_server_pointer: true,
        pointer_software_rendering: true,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        compression_type: None,
        multitransport_flags: None,
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        performance_flags: PerformanceFlags::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(input: ClientMsg, last_pos: &mut (u16, u16)) -> FastPathInputEvent {
        let mut events = translate_input(input, last_pos);
        assert_eq!(events.len(), 1, "expected exactly one event");
        events.remove(0)
    }

    #[test]
    fn mouse_move_sets_flags_and_updates_last_pos() {
        let mut pos = (0, 0);
        let event = one(ClientMsg::MouseMove { x: 40, y: 50 }, &mut pos);
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert_eq!(pdu.flags, PointerFlags::MOVE);
                assert_eq!((pdu.x_position, pdu.y_position), (40, 50));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(pos, (40, 50));
    }

    #[test]
    fn negative_and_huge_coords_are_clamped() {
        let mut pos = (0, 0);
        let event = one(ClientMsg::MouseMove { x: -3, y: 100_000 }, &mut pos);
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert_eq!((pdu.x_position, pdu.y_position), (0, u16::MAX));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn button_press_uses_last_pos_and_down_flag() {
        let mut pos = (12, 34);
        let event = one(
            ClientMsg::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            &mut pos,
        );
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert!(pdu.flags.contains(PointerFlags::LEFT_BUTTON));
                assert!(pdu.flags.contains(PointerFlags::DOWN));
                assert_eq!((pdu.x_position, pdu.y_position), (12, 34));
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Release drops the DOWN flag.
        let event = one(
            ClientMsg::MouseButton {
                button: MouseButton::Right,
                pressed: false,
            },
            &mut pos,
        );
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert!(pdu.flags.contains(PointerFlags::RIGHT_BUTTON));
                assert!(!pdu.flags.contains(PointerFlags::DOWN));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn wheel_down_is_negative_vertical() {
        let mut pos = (0, 0);
        let event = one(ClientMsg::Wheel { dx: 0.0, dy: 3.0 }, &mut pos);
        match event {
            FastPathInputEvent::MouseEvent(pdu) => {
                assert!(pdu.flags.contains(PointerFlags::VERTICAL_WHEEL));
                assert_eq!(pdu.number_of_wheel_rotation_units, -120);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn key_maps_scancode_release_and_extended() {
        let mut pos = (0, 0);

        match one(
            ClientMsg::Key {
                code: "KeyA".to_owned(),
                pressed: true,
            },
            &mut pos,
        ) {
            FastPathInputEvent::KeyboardEvent(flags, code) => {
                assert_eq!(code, 0x1E);
                assert!(flags.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }

        match one(
            ClientMsg::Key {
                code: "ArrowUp".to_owned(),
                pressed: false,
            },
            &mut pos,
        ) {
            FastPathInputEvent::KeyboardEvent(flags, code) => {
                assert_eq!(code, 0x48);
                assert!(flags.contains(KeyboardFlags::RELEASE));
                assert!(flags.contains(KeyboardFlags::EXTENDED));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn host_port_brackets_ipv6_literals() {
        assert_eq!(host_port("192.0.2.10", 3389), "192.0.2.10:3389");
        assert_eq!(host_port("desktop-vnvgdaf", 3389), "desktop-vnvgdaf:3389");
        assert_eq!(
            host_port("fdb8:d92a:f690:3d7f:97a4:120a:2:20", 3389),
            "[fdb8:d92a:f690:3d7f:97a4:120a:2:20]:3389"
        );
        // Already-bracketed input is left as-is.
        assert_eq!(host_port("[fdb8::20]", 3389), "[fdb8::20]:3389");
    }

    #[test]
    fn unmapped_key_produces_no_events() {
        let mut pos = (0, 0);
        assert!(
            translate_input(
                ClientMsg::Key {
                    code: "Nonexistent".to_owned(),
                    pressed: true,
                },
                &mut pos,
            )
            .is_empty()
        );
    }
}
