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
use crate::gfx::{GpuArea, GpuPaint, Graphics, Plan, Update};
use crate::proto::wire::Malformed;

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
    /// What the page draws from the pictures it shows from its GPU
    /// ([`Compositor::show_on_gpu`]), after the painted rectangles: the
    /// framebuffer does not hold them.
    pub gpu: Vec<GpuPaint>,
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

    /// One message of a session's `Event::Graphics`, as the host sent it, out of
    /// its bulk compression: the commands [`Self::compose`] takes.
    ///
    /// Each message is compressed against every one before it on the channel, so
    /// this takes every message of the pipeline, in order, from its first. An
    /// error is a message that does not unwrap, after which none after it can be.
    pub fn unwrap(&mut self, wrapped: &[u8]) -> Result<Vec<u8>> {
        let mut commands = Vec::new();
        self.graphics.unwrap(wrapped, &mut commands)?;
        Ok(commands)
    }

    /// Compose one run of commands: a message [`Self::unwrap`] has unwrapped,
    /// whole PDUs.
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
                Update::Confirmed | Update::Passed { .. } | Update::Drawn(_) => {}
            }
        }
        composed.gpu = self.graphics.take_gpu_paints();
        Ok(composed)
    }

    /// What a run of commands, unwrapped, asks of whoever composes it first: which
    /// of its access units may be shown from the GPU, and what of what the GPU
    /// shows it reads, to be restored. See [`crate::gfx`]. An error is a command
    /// that does not decode, which [`Self::compose`] refuses too.
    pub fn plan(&self, commands: &[u8]) -> Result<Plan, Malformed> {
        self.graphics.plan(commands)
    }

    /// Access unit `unit` of the run composed next is shown from the GPU rather
    /// than supplied: its rectangles are left to whoever composes the pipeline to
    /// draw, as [`Composed::gpu`] says. Only for a unit [`Self::plan`] says may be.
    pub fn show_on_gpu(&mut self, unit: u32) {
        self.graphics.show_on_gpu(unit);
    }

    /// Every rectangle shown from the GPU, where the output shows it.
    pub fn gpu_areas(&self) -> Vec<GpuArea> {
        self.graphics.gpu_areas()
    }

    /// A rectangle shown from the GPU, read back: `rgbx` is `rect`'s rows of the
    /// surface's pixels, packed, top row first, read off the output where
    /// [`GpuArea::output`] says.
    pub fn restore(&mut self, surface: u16, rect: Rect, rgbx: &[u8]) -> Result<()> {
        self.graphics.restore(surface, rect, rgbx)
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
