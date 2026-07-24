//! Wire protocol shared (in shape) with the frontend `src/protocol.ts`.
//!
//! `ClientMsg` flows browser -> server (input events) as JSON text frames.
//! Server -> browser, the transport is split by weight (see
//! docs/architecture.md):
//!
//! - **Screen tiles** are binary WebSocket frames: a fixed 10-byte header
//!   followed by a PNG-compressed payload. This replaced base64 RGBA inside
//!   JSON text, which inflated the bottleneck backend->browser link by ~4.3x
//!   (4 bytes/px, +33% base64).
//! - **Control messages** (`resize`, `error`) are rare and tiny; they stay
//!   JSON text frames with a `type` tag.

use serde::{Deserialize, Serialize};

/// Transport policy shared by all engines: a dirty rectangle taller than this
/// is split into strips before being sent, so a full-screen repaint doesn't
/// produce one huge WebSocket message.
pub const STRIP_ROWS: u16 = 64;

/// A mouse button, matching the DOM `MouseEvent.button` numbering.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Browser -> server: input events captured over the remote canvas.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMsg {
    /// Pointer moved to framebuffer coordinates (x, y).
    MouseMove { x: i32, y: i32 },
    /// A mouse button was pressed or released.
    MouseButton { button: MouseButton, pressed: bool },
    /// Scroll wheel delta.
    Wheel { dx: f32, dy: f32 },
    /// A key was pressed or released. `code` is the DOM `KeyboardEvent.code`.
    Key { code: String, pressed: bool },
    /// The browser viewport, in device pixels — the size the browser wants
    /// the remote desktop to be. Engines that can drive the remote
    /// size act on it (VNC `SetDesktopSize`); the rest ignore it and the
    /// frontend keeps its scrollbars.
    Viewport { w: u16, h: u16 },
    /// Re-announce the desktop size and repaint the whole framebuffer.
    /// Injected by the session layer when a browser (re)attaches to
    /// a running engine; a browser may also send it to recover from a
    /// corrupted canvas.
    Refresh,
}

/// A dirty rectangle of the framebuffer, sent as one binary WebSocket frame.
/// The payload is always a PNG stream (every browser decodes PNG natively);
/// PNG's worst case over raw is a fraction of a percent, so a raw format is
/// not worth a second decode path.
///
/// Frame layout (little-endian):
///
/// ```text
/// offset 0: u8  frame kind, always 0x01 (tile)
/// offset 1: u8  format, always 1 (PNG) — reserved for a future codec
/// offset 2: u16 x
/// offset 4: u16 y
/// offset 6: u16 w
/// offset 8: u16 h
/// offset 10: payload (a PNG stream)
/// ```
#[derive(Debug, Clone)]
pub struct Tile {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// PNG-encoded RGB image.
    pub data: Vec<u8>,
}

impl Tile {
    pub const FRAME_KIND: u8 = 0x01;
    pub const FORMAT_PNG: u8 = 1;
    pub const HEADER_LEN: usize = 10;

    /// Build a tile from packed RGB888 pixels, PNG-compressing the payload.
    pub fn from_rgb(x: u16, y: u16, w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Self> {
        let expected = usize::from(w) * usize::from(h) * 3;
        anyhow::ensure!(
            rgb.len() == expected,
            "tile payload is {} bytes, expected {expected} for {w}x{h} RGB",
            rgb.len()
        );
        let data = encode_png(w, h, rgb)?;
        Ok(Self { x, y, w, h, data })
    }

    /// Serialize into the binary WebSocket frame described above.
    pub fn to_frame(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.data.len());
        out.push(Self::FRAME_KIND);
        out.push(Self::FORMAT_PNG);
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&self.h.to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }
}

/// PNG-encode packed RGB888 pixels. Fast compression: the win over raw is
/// already large for screen content, and this runs on the session's hot path.
fn encode_png(w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, u32::from(w), u32::from(h));
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgb)?;
    writer.finish()?;
    Ok(out)
}

/// Server -> browser: screen updates and status.
#[derive(Debug, Clone)]
pub enum ServerMsg {
    Tile(Tile),
    /// The remote desktop resolution changed.
    Resize { w: u16, h: u16 },
    /// A fatal session error the client should surface.
    Error { message: String },
}

/// One encoded WebSocket frame, ready to send.
#[derive(Debug)]
pub enum WireFrame {
    Text(String),
    Binary(Vec<u8>),
}

/// JSON shape of the text-frame control messages (`ServerMsg` minus tiles).
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ControlMsg<'a> {
    Resize { w: u16, h: u16 },
    Error { message: &'a str },
}

impl ServerMsg {
    /// Encode for the WebSocket: tiles as binary frames, control as JSON text.
    pub fn encode(&self) -> WireFrame {
        match self {
            ServerMsg::Tile(tile) => WireFrame::Binary(tile.to_frame()),
            ServerMsg::Resize { w, h } => WireFrame::Text(control(&ControlMsg::Resize { w: *w, h: *h })),
            ServerMsg::Error { message } => WireFrame::Text(control(&ControlMsg::Error { message })),
        }
    }
}

fn control(msg: &ControlMsg<'_>) -> String {
    // Infallible: ControlMsg is a string/number-only struct enum.
    serde_json::to_string(msg).expect("control message serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deserialize the exact JSON the frontend (`protocol.ts`) sends.
    #[test]
    fn client_messages_deserialize_from_frontend_json() {
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"mouseMove","x":5,"y":6}"#).unwrap(),
            ClientMsg::MouseMove { x: 5, y: 6 }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(
                r#"{"type":"mouseButton","button":"right","pressed":true}"#
            )
            .unwrap(),
            ClientMsg::MouseButton {
                button: MouseButton::Right,
                pressed: true
            }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"wheel","dx":0.0,"dy":-2.5}"#).unwrap(),
            ClientMsg::Wheel { dy, .. } if dy == -2.5
        ));
        match serde_json::from_str::<ClientMsg>(r#"{"type":"key","code":"KeyA","pressed":false}"#)
            .unwrap()
        {
            ClientMsg::Key { code, pressed } => {
                assert_eq!(code, "KeyA");
                assert!(!pressed);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"viewport","w":2560,"h":1440}"#).unwrap(),
            ClientMsg::Viewport { w: 2560, h: 1440 }
        ));
        // Viewport dimensions beyond the protocol's u16 range are rejected at
        // the deserialization boundary, not clamped.
        assert!(serde_json::from_str::<ClientMsg>(r#"{"type":"viewport","w":70000,"h":1}"#).is_err());
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"type":"refresh"}"#).unwrap(),
            ClientMsg::Refresh
        ));
    }

    // Control messages keep the tagged, camelCase text shape `protocol.ts` expects.
    #[test]
    fn control_messages_encode_to_tagged_camelcase_text() {
        match (ServerMsg::Resize { w: 1280, h: 800 }).encode() {
            WireFrame::Text(json) => assert_eq!(json, r#"{"type":"resize","w":1280,"h":800}"#),
            other => panic!("resize should be a text frame: {other:?}"),
        }
        match (ServerMsg::Error { message: "boom".to_owned() }).encode() {
            WireFrame::Text(json) => assert_eq!(json, r#"{"type":"error","message":"boom"}"#),
            other => panic!("error should be a text frame: {other:?}"),
        }
    }

    // The binary layout `protocol.ts` (decodeTileFrame) parses.
    #[test]
    fn tile_frame_layout_is_kind_format_le_coords_payload() {
        let tile = Tile {
            x: 0x0102,
            y: 0x0304,
            w: 2,
            h: 1,
            data: vec![10, 20, 30, 40, 50, 60],
        };
        let frame = tile.to_frame();
        assert_eq!(frame[0], Tile::FRAME_KIND);
        assert_eq!(frame[1], Tile::FORMAT_PNG);
        assert_eq!(&frame[2..4], &[0x02, 0x01]); // x, little-endian
        assert_eq!(&frame[4..6], &[0x04, 0x03]); // y
        assert_eq!(&frame[6..8], &[2, 0]); // w
        assert_eq!(&frame[8..10], &[1, 0]); // h
        assert_eq!(&frame[10..], &[10, 20, 30, 40, 50, 60]);

        match (ServerMsg::Tile(tile)).encode() {
            WireFrame::Binary(bytes) => assert_eq!(bytes, frame),
            other => panic!("tile should be a binary frame: {other:?}"),
        }
    }

    /// A desktop-like strip: horizontal gradient, repeated rows.
    fn gradient_rgb(w: u16, h: u16) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for _ in 0..h {
            for x in 0..w {
                let v = (x % 256) as u8;
                rgb.extend_from_slice(&[v, v / 2, 255 - v]);
            }
        }
        rgb
    }

    #[test]
    fn screen_content_compresses_to_png_and_roundtrips() {
        let (w, h) = (320, 64);
        let rgb = gradient_rgb(w, h);
        let tile = Tile::from_rgb(7, 9, w, h, &rgb).unwrap();
        assert!(
            tile.data.len() < rgb.len() / 4,
            "PNG should compress a gradient well: {} vs raw {}",
            tile.data.len(),
            rgb.len()
        );

        // Decode the PNG back and verify the pixels survived.
        let decoder = png::Decoder::new(std::io::Cursor::new(tile.data.as_slice()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (u32::from(w), u32::from(h)));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(&buf[..info.buffer_size()], rgb.as_slice());
    }

    // The binary tile frame's reason to exist: it must beat the old
    // base64-in-JSON baseline by a wide margin for screen-like content.
    #[test]
    fn tile_frame_beats_old_base64_json_baseline() {
        let (w, h) = (1280, 64);
        let rgb = gradient_rgb(w, h);
        let frame = Tile::from_rgb(0, 0, w, h, &rgb).unwrap().to_frame();
        // Old wire cost: RGBA (4 bytes/px) -> base64 (4/3) + ~90 bytes of JSON.
        let old = usize::from(w) * usize::from(h) * 4 * 4 / 3 + 90;
        assert!(
            frame.len() * 10 < old,
            "expected >10x reduction: {} vs baseline {old}",
            frame.len()
        );
    }

    #[test]
    fn tiny_tile_is_still_a_valid_png() {
        // 2x2 of "noise" — PNG's fixed overhead dominates here, which is
        // accepted: one decode path beats saving a few dozen bytes.
        let rgb = [1u8, 200, 3, 250, 5, 90, 7, 160, 9, 30, 11, 220];
        let tile = Tile::from_rgb(0, 0, 2, 2, &rgb).unwrap();
        assert_eq!(&tile.data[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn tile_with_wrong_payload_length_is_rejected() {
        assert!(Tile::from_rgb(0, 0, 2, 2, &[0u8; 5]).is_err());
    }
}
