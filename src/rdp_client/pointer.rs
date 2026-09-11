//! The remote mouse cursor.
//!
//! RDP sends a cursor as a pair of AND and XOR masks — the encoding Windows has
//! used since it drew cursors by XORing into the framebuffer — and IronRDP decodes
//! them. Asked for the *accelerated* target, as this client always does, it hands
//! back straight-alpha RGBA ready for a compositor rather than drawing the pointer
//! into the desktop, which is what lets a browser wear it on its own hardware
//! pointer.

use std::fmt;

use ironrdp::graphics::pointer::DecodedPointer;
use log::warn;

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

/// The largest cursor this will hand on.
///
/// RDP's own limit is 384×384. This is a gate on what a consumer is asked to hold,
/// sitting behind a decoder that sized its output from numbers that arrived over
/// the network, not a second check of protocol validity.
const MAX_DIMENSION: u16 = 384;

/// A decoded pointer as the image this crate hands out, or `None` for one that is
/// empty, oversized, or not the size its own header claims.
pub(super) fn image(pointer: &DecodedPointer) -> Option<CursorImage> {
    let (width, height) = (pointer.width, pointer.height);
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        warn!("rdp: refusing a {width}x{height} cursor (limit {MAX_DIMENSION})");
        return None;
    }
    if pointer.bitmap_data.len() != usize::from(width) * usize::from(height) * 4 {
        warn!(
            "rdp: refusing a {width}x{height} cursor carrying {} bytes",
            pointer.bitmap_data.len()
        );
        return None;
    }
    Some(CursorImage {
        width: u32::from(width),
        height: u32::from(height),
        hotspot_x: u32::from(pointer.hotspot_x),
        hotspot_y: u32::from(pointer.hotspot_y),
        rgba: pointer.bitmap_data.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(width: u16, height: u16, bytes: usize) -> DecodedPointer {
        DecodedPointer { width, height, hotspot_x: 3, hotspot_y: 4, bitmap_data: vec![7; bytes] }
    }

    #[test]
    fn a_pointer_keeps_its_hotspot_and_pixels() {
        let image = image(&decoded(2, 3, 24)).expect("a well-formed pointer");
        assert_eq!((image.width, image.height, image.hotspot_x, image.hotspot_y), (2, 3, 3, 4));
        assert_eq!(image.rgba, vec![7; 24]);
    }

    #[test]
    fn a_pointer_that_is_not_its_own_size_is_refused() {
        assert!(image(&decoded(0, 3, 0)).is_none(), "empty");
        assert!(image(&decoded(MAX_DIMENSION + 1, 1, usize::from(MAX_DIMENSION + 1) * 4)).is_none());
        assert!(image(&decoded(2, 3, 23)).is_none(), "short of its header");
    }

    /// A 384×384 cursor must not print 590 KB of integers into a log line.
    #[test]
    fn debug_prints_the_size_not_the_pixels() {
        let text = format!("{:?}", image(&decoded(2, 3, 24)).unwrap());
        assert_eq!(text, "CursorImage { 2x3, hotspot 3,4, 24 bytes }");
    }
}
