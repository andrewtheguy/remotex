//! The remote mouse cursor, as a caller sees it.
//!
//! [`super::proto::pointer`] does the decoding — the AND and XOR masks Windows has
//! used since it drew cursors by XORing into the framebuffer, turned into
//! straight-alpha `RGBA`. This is the shape that leaves this crate: the same pixels,
//! counted in `u32` like every other size a caller of [`super::Session`] is handed,
//! and never drawn into the desktop, which is what lets a browser wear the pointer on
//! its own hardware cursor.

use std::fmt;

use super::proto::pointer::Shape;

/// A cursor bitmap, in straight-alpha `RGBA`.
#[derive(Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// Where the click actually lands, relative to the top left of the image.
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// `width * height * 4` bytes: R, G, B, A.
    pub rgba: Vec<u8>,
}

/// Hand-written, because the derived one prints every byte.
///
/// `Cursor` is carried inside an `Event`, and the obvious thing to do with an
/// unexpected event is `{:?}` it into a log. A 384×384 cursor derives to about
/// 2.3 MB of comma-separated integers — enough to make the one line somebody needed
/// unfindable. The size and hotspot are what a reader wants; the pixels are not.
impl fmt::Debug for CursorImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CursorImage {{ {}x{}, hotspot {},{}, {} bytes }}",
            self.width,
            self.height,
            self.hotspot_x,
            self.hotspot_y,
            self.rgba.len()
        )
    }
}

/// A decoded shape, which the decoder has already bounded to RDP's own 384×384 and
/// sized to its own dimensions — so there is nothing left to check here.
impl From<Shape> for CursorImage {
    fn from(shape: Shape) -> Self {
        Self {
            width: u32::from(shape.width),
            height: u32::from(shape.height),
            hotspot_x: u32::from(shape.hotspot_x),
            hotspot_y: u32::from(shape.hotspot_y),
            rgba: shape.rgba,
        }
    }
}

/// What the pointer should look like now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cursor {
    /// The server asked for no cursor at all — a full-screen video player, say.
    Hidden,
    /// The server asked for the system default arrow, without sending a bitmap for
    /// it. There is nothing to draw here; show whatever the local platform calls a
    /// default pointer.
    Default,
    /// A bitmap.
    Image(CursorImage),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(width: u16, height: u16) -> Shape {
        let rgba = vec![7; usize::from(width) * usize::from(height) * 4];
        Shape { width, height, hotspot_x: 3, hotspot_y: 4, rgba }
    }

    #[test]
    fn a_pointer_keeps_its_hotspot_and_pixels() {
        let image = CursorImage::from(shape(2, 3));
        assert_eq!((image.width, image.height, image.hotspot_x, image.hotspot_y), (2, 3, 3, 4));
        assert_eq!(image.rgba, vec![7; 24]);
    }

    /// A 384×384 cursor must not print 590 KB of integers into a log line.
    #[test]
    fn debug_prints_the_size_not_the_pixels() {
        let text = format!("{:?}", CursorImage::from(shape(2, 3)));
        assert_eq!(text, "CursorImage { 2x3, hotspot 3,4, 24 bytes }");
    }
}
