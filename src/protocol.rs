//! Wire protocol shared (in shape) with the frontend `src/protocol.ts`.
//!
//! Messages are JSON with a `type` tag. `ClientMsg` flows browser -> server
//! (input events); `ServerMsg` flows server -> browser (screen updates).
//! Kept minimal for the MVP — see docs/phase1-mvp.md.
//!
//! `ServerMsg` and friends are unused by the skeleton (nothing sends frames
//! yet); they define the Phase 1 seam.

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
