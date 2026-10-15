//! A graphics pipeline composed from commands that were passed on, and a desktop
//! composed from bitmap updates that were.
//!
//! A session told to pass its graphics on (`Connect::pass_graphics`, in the
//! gateway's `rdp_client`) hands its pipeline's commands to its caller instead of
//! composing them. This is the other half: the same compositor the session would have run — every codec, the surfaces
//! and the caches — fed those commands by whoever they were passed to. It is what
//! the page's WebAssembly module runs (`frontend/wasm/egfx`), and what a test
//! that reads a passed pipeline composes it with.
//!
//! A compositor starts with nothing and is only ever right for a pipeline it has
//! followed from its first command: the host draws against what its client already
//! holds. A [`BitmapCompositor`] has no such past: each rectangle is the pixels
//! that go there.

use anyhow::Result;

use crate::avc::Picture;
use crate::framebuffer::{Framebuffer, Rect, affordable};
use crate::gfx::{Graphics, Update};
use crate::proto::bitmap::{self, Scratch};

/// What one run of commands did to the picture.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Composed {
    /// The output's new size, when the run reset it. The framebuffer is that size
    /// already, and blank but for what the run drew after.
    pub resized: Option<(u32, u32)>,
    /// The rectangles of the framebuffer the run painted, in the order it painted
    /// them. Those from before a reset in the same run name a framebuffer that is
    /// gone, and are left out.
    pub painted: Vec<Rect>,
    /// How many frames the run ended.
    pub frames: u32,
}

/// The pipeline's compositor, and the framebuffer it composes into.
pub struct Compositor {
    graphics: Graphics,
    framebuffer: Framebuffer,
}

impl Default for Compositor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compositor {
    pub fn new() -> Self {
        Self { graphics: Graphics::new(), framebuffer: Framebuffer::new() }
    }

    /// Compose one run of commands, as a session's `Event::Graphics`
    /// carries them: whole PDUs, out of their bulk compression.
    ///
    /// An error is a command that does not decode, after which the pipeline's state
    /// is not the host's and nothing composed from it can be trusted. A codec
    /// payload that does not decode is not one: that rectangle is left as it was.
    pub fn compose(&mut self, commands: &[u8]) -> Result<Composed> {
        let mut composed = Composed::default();
        for update in self.graphics.compose(commands, &self.framebuffer)? {
            match update {
                Update::Reset { width, height, .. } => {
                    composed.resized = Some((width, height));
                    composed.painted.clear();
                }
                Update::Paint(rect) => composed.painted.push(rect),
                Update::Frame { .. } => composed.frames += 1,
                Update::Confirmed | Update::Passed { .. } => {}
            }
        }
        Ok(composed)
    }

    /// The decoded picture of one H.264 access unit in the run composed next:
    /// `unit` is its number in that run, as [`crate::avc::scan`] counts them. The
    /// run's commands take the pictures in their turn, and one left over is dropped
    /// with the run.
    ///
    /// A host sends H.264 only to a session that advertised it, and decoding it is
    /// whoever composes the pipeline's to do — see [`crate::avc`].
    pub fn supply(&mut self, unit: u32, picture: Picture) {
        self.graphics.supply(unit, picture);
    }

    /// The picture as composed so far.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }
}

/// TESTING ONLY: a desktop drawn with bitmap updates, composed from the updates a
/// session passed on (`Connect::pass_bitmaps` in the gateway's `rdp_client`).
///
/// It holds nothing a later update draws against — no surface, no cache, no codec
/// state — so it can start at any update, and a desktop it joined late is whole
/// again once the host repaints it.
pub struct BitmapCompositor {
    framebuffer: Framebuffer,
    scratch: Scratch,
    /// One rectangle's pixels on their way to the framebuffer.
    pixels: Vec<u8>,
}

impl BitmapCompositor {
    /// A blank desktop of `width` by `height`: the size the session connected at,
    /// which a desktop drawn with bitmap updates keeps.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        affordable(width, height)?;
        let framebuffer = Framebuffer::new();
        framebuffer.resize(width, height);
        Ok(Self { framebuffer, scratch: Scratch::default(), pixels: Vec::new() })
    }

    /// Paint one run of Bitmap Updates laid end to end ([`bitmap::updates`]).
    ///
    /// An error is a run that does not parse or a rectangle that does not decode,
    /// as it ends the session that decodes them in the gateway.
    pub fn compose(&mut self, run: &[u8]) -> Result<Composed> {
        let mut composed = Composed::default();
        for rectangle in bitmap::updates(run)? {
            rectangle.decode(&mut self.scratch, &mut self.pixels)?;
            let rect = Rect {
                x: u32::from(rectangle.x),
                y: u32::from(rectangle.y),
                width: u32::from(rectangle.paint_width),
                height: u32::from(rectangle.paint_height),
            };
            if !rect.is_empty() && self.framebuffer.blit(&self.pixels, rect) {
                composed.painted.push(rect);
            }
        }
        Ok(composed)
    }

    /// The desktop as painted so far.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One Bitmap Update of uncompressed rectangles, each `(x, y, w, h, BGRX)`.
    fn update(rectangles: &[(u16, u16, u16, u16, [u8; 4])]) -> Vec<u8> {
        let mut body = 1_u16.to_le_bytes().to_vec();
        body.extend_from_slice(&u16::try_from(rectangles.len()).unwrap().to_le_bytes());
        for &(x, y, w, h, bgrx) in rectangles {
            for field in [x, y, x + w - 1, y + h - 1, w, h, bitmap::DEPTH, 0] {
                body.extend_from_slice(&field.to_le_bytes());
            }
            let pixels = bgrx.repeat(usize::from(w) * usize::from(h));
            body.extend_from_slice(&u16::try_from(pixels.len()).unwrap().to_le_bytes());
            body.extend_from_slice(&pixels);
        }
        body
    }

    #[test]
    fn a_run_of_bitmap_updates_paints_each_rectangle_where_it_goes() {
        let mut desktop = BitmapCompositor::new(128, 64).unwrap();
        let mut run = update(&[(0, 0, 64, 64, [0x30, 0x20, 0x10, 0])]);
        run.extend(update(&[(64, 0, 64, 32, [3, 2, 1, 0])]));
        let composed = desktop.compose(&run).unwrap();
        assert_eq!(
            composed.painted,
            [Rect { x: 0, y: 0, width: 64, height: 64 }, Rect { x: 64, y: 0, width: 64, height: 32 }]
        );
        assert_eq!(composed.resized, None);
        desktop.framebuffer().with(|frame| {
            // RGBX, red first: the wire's BGRX turned round.
            assert_eq!(&frame.pixels[..4], &[0x10, 0x20, 0x30, 0]);
            let right = 64 * 4;
            assert_eq!(&frame.pixels[right..right + 4], &[1, 2, 3, 0]);
            let below = 40 * frame.stride + right;
            assert_eq!(&frame.pixels[below..below + 4], &[0, 0, 0, 0], "not painted");
        });
    }

    #[test]
    fn a_run_that_does_not_parse_is_refused() {
        let mut desktop = BitmapCompositor::new(64, 64).unwrap();
        let run = update(&[(0, 0, 2, 2, [0; 4])]);
        assert!(desktop.compose(&run[..run.len() - 1]).is_err());
    }
}
