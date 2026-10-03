//! A graphics pipeline composed from commands that were passed on.
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
//! holds.

use anyhow::Result;

use crate::avc::Picture;
use crate::framebuffer::{Framebuffer, Rect};
use crate::gfx::{Graphics, Update};

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
