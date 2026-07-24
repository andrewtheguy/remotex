//! Wire protocol shared (in shape) with the frontend `src/protocol.ts`.
//!
//! Messages are JSON with a `type` tag. `ClientMsg` flows browser -> server
//! (input events); `ServerMsg` flows server -> browser (screen updates).
//! Kept minimal for the MVP — see docs/phase1-mvp.md.

// `TileFormat::Png` is defined for the protocol but the server only emits `Rgba`
// tiles today; allow the unused variant.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

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
}

/// Pixel format for a tile payload.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TileFormat {
    /// Raw RGBA8888, row-major, `w * h * 4` bytes.
    Rgba,
    /// PNG-encoded image.
    Png,
}

/// Server -> browser: screen updates and status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMsg {
    /// A dirty rectangle of the framebuffer. `data` is base64-encoded.
    Tile {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        format: TileFormat,
        data: String,
    },
    /// The remote desktop resolution changed.
    Resize { w: i32, h: i32 },
    /// A fatal session error the client should surface.
    Error { message: String },
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
    }

    // Serialize into the tagged, camelCase shape `protocol.ts` expects.
    #[test]
    fn server_messages_serialize_to_tagged_camelcase() {
        let tile = ServerMsg::Tile {
            x: 1,
            y: 2,
            w: 3,
            h: 4,
            format: TileFormat::Rgba,
            data: "AAAA".to_owned(),
        };
        let json = serde_json::to_string(&tile).unwrap();
        assert!(json.contains(r#""type":"tile""#), "{json}");
        assert!(json.contains(r#""format":"rgba""#), "{json}");
        assert!(json.contains(r#""data":"AAAA""#), "{json}");

        assert_eq!(
            serde_json::to_string(&ServerMsg::Resize { w: 1280, h: 800 }).unwrap(),
            r#"{"type":"resize","w":1280,"h":800}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMsg::Error {
                message: "boom".to_owned()
            })
            .unwrap(),
            r#"{"type":"error","message":"boom"}"#
        );
    }
}
