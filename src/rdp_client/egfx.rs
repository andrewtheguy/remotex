//! The graphics pipeline (MS-RDPEGFX), as a channel that reports in order.
//!
//! IronRDP's [`GraphicsPipelineClient`] does the protocol: capability exchange,
//! surfaces, the codecs, the cache, frame acknowledgment, and a compositor that
//! folds every surface command into the output and hands out the regions each
//! completed frame changed. What it does not do is say *when* the output was
//! redefined relative to those regions. `ActiveStage` drains the compositor into a
//! decoded image it was handed at connect and never resizes, so after a graphics
//! reset — which is how a Windows host answers a monitor layout under the pipeline
//! — every region outside the old desktop is dropped on the floor.
//!
//! So the client is wrapped rather than registered directly. `ActiveStage` looks
//! the pipeline up by type, finds nothing, and leaves it alone; this wrapper drains
//! the compositor itself after every message and queues what happened as
//! [`Update`]s, in the order the session must apply them: a reset first (the
//! compositor has already discarded everything that preceded it), then the
//! message's regions, then the frame boundary.

use std::sync::{Arc, Mutex, MutexGuard};

use ironrdp::core::impl_as_any;
use ironrdp::dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp::pdu::PduResult;
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};

use super::framebuffer::Rect;

/// Something the pipeline did to the desktop.
pub(super) enum Update {
    /// The server redefined the graphics output (`ResetGraphics`): every surface is
    /// gone and the desktop is now this size.
    Reset { width: u32, height: u32 },
    /// A completed frame changed this rectangle of the output to these pixels,
    /// tightly packed RGBA.
    Paint { rect: Rect, rgba: Vec<u8> },
    /// The frames whose regions were just queued are complete.
    FrameEnd,
}

/// The queue between the channel, which runs inside `ActiveStage::process`, and
/// the session loop that called it.
///
/// Shared rather than returned because the channel is owned by IronRDP's dynamic
/// channel set and is only ever called from inside it; the lock is never contended
/// — both sides run on the session thread — and exists to satisfy `Send`.
#[derive(Clone, Default)]
pub(super) struct Updates(Arc<Mutex<Vec<Update>>>);

impl Updates {
    pub(super) fn take(&self) -> Vec<Update> {
        std::mem::take(&mut *self.lock())
    }

    fn push(&self, update: Update) {
        self.lock().push(update);
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Update>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(super) struct Channel {
    inner: GraphicsPipelineClient,
    updates: Updates,
}

impl Channel {
    /// The pipeline with no H.264 decoder, which makes IronRDP advertise only the
    /// capability versions that do not require one — so the server never picks an
    /// AVC codec this end would have to drop.
    pub(super) fn new(updates: Updates) -> Self {
        let handler = Resets { updates: updates.clone() };
        Self { inner: GraphicsPipelineClient::new(Box::new(handler), None), updates }
    }
}

/// The one handler callback this needs: the reset, queued at the moment it
/// happens, which is ahead of every region the same message goes on to produce.
struct Resets {
    updates: Updates,
}

impl GraphicsPipelineHandler for Resets {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        self.updates.push(Update::Reset { width, height });
    }
}

impl_as_any!(Channel);

impl DvcProcessor for Channel {
    fn channel_name(&self) -> &str {
        self.inner.channel_name()
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.inner.start(channel_id)
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let responses = self.inner.process(channel_id, payload)?;
        let regions = self.inner.drain_output();
        if !regions.is_empty() {
            for region in regions {
                let r = region.region;
                let rect = Rect {
                    x: u32::from(r.left),
                    y: u32::from(r.top),
                    width: u32::from(r.right.saturating_sub(r.left)),
                    height: u32::from(r.bottom.saturating_sub(r.top)),
                };
                if !rect.is_empty() {
                    self.updates.push(Update::Paint { rect, rgba: region.data });
                }
            }
            // Regions are only ever committed by an `EndFrame`, so a message that
            // produced any finished at least one frame.
            self.updates.push(Update::FrameEnd);
        }
        Ok(responses)
    }

    fn close(&mut self, channel_id: u32) {
        self.inner.close(channel_id);
    }
}

impl DvcClientProcessor for Channel {}
