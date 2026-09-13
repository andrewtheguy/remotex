//! The camera side of a [`Session`](super::Session): a feed the caller hands a device and
//! its samples to, and a sink the session tells the host's decisions to.
//!
//! The protocol is [`rdpecam`](super::proto::rdpecam), run on the session thread beside
//! everything else on the connection; this is the way in and the way out. The way in is
//! two queues rather than one, because the two kinds of traffic want opposite things
//! when the session thread is behind: plugging and unplugging must never be lost, and a
//! sample is worthless once it is late. So the device's commands queue without limit,
//! and a sample that finds its own bounded queue full is dropped here — with every
//! sample after it until a keyframe, because H.264 cannot resume from the middle of a
//! group of pictures.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc::{self, error::TrySendError};

use super::proto::rdpecam::Format;

/// How many samples may wait for the session thread.
///
/// The thread drains the queue between PDUs and meters the samples out by the host's
/// requests itself, so a full queue is a thread busy elsewhere — a resize, a burst of
/// desktop — for long enough that half a second of camera is already late.
const SAMPLE_QUEUE: usize = 16;

/// A camera for a session to offer: the name the host shows beside it, and where the
/// host's decisions about it go.
pub struct Camera {
    pub name: String,
    pub sink: Box<dyn CameraSink>,
}

/// Where the host's camera decisions go.
///
/// Called on the session's thread, in step with the desktop being decoded, so nothing
/// here may wait.
pub trait CameraSink: Send {
    /// The host agreed a protocol version, so camera redirection is on offer. A host that
    /// never says this does not create the enumeration channel at all — a policy that
    /// turns it off, or a Windows Server without the Remote Desktop Session Host role.
    fn negotiated(&self, version: u8);
    /// The host opened the device's channel: the camera is being installed over there.
    fn attached(&self);
    /// An application on the host started the stream: samples in `format` are wanted,
    /// from a keyframe on.
    fn started(&self, format: Format);
    /// The stream stopped. The device stays plugged, and another start may follow.
    fn stopped(&self);
    /// Samples were dropped, and the next one must be a keyframe.
    fn keyframe_needed(&self);
}

pub(super) enum CameraCommand {
    Plug(Format),
    Unplug,
}

pub(super) struct Sample {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// One thing the caller's feed wants done, as the session thread takes it.
pub(super) enum CameraInput {
    Command(CameraCommand),
    Sample(Sample),
}

/// The session thread's end of a [`CameraFeed`].
pub(super) struct CameraQueues {
    commands: mpsc::UnboundedReceiver<CameraCommand>,
    samples: mpsc::Receiver<Sample>,
}

impl CameraQueues {
    /// The next thing the caller wants, or `None` once every feed is gone.
    pub(super) async fn next(&mut self) -> Option<CameraInput> {
        tokio::select! {
            // Biased so a plug or an unplug goes before samples queued behind it.
            biased;
            Some(command) = self.commands.recv() => Some(CameraInput::Command(command)),
            Some(sample) = self.samples.recv() => Some(CameraInput::Sample(sample)),
            else => None,
        }
    }
}

/// What became of a sample handed to [`CameraFeed::sample`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fed {
    /// On its way to the session thread.
    Queued,
    /// Dropped for a full queue, opening a gap: the caller owes a keyframe.
    Dropped,
    /// Dropped inside a gap already reported: nothing new is owed.
    Skipped,
    /// The session has ended.
    Ended,
}

/// The camera side of a [`Session`](super::Session), for a session configured with
/// [`Connect::camera`](super::Connect::camera).
///
/// Cheap to clone, and every clone feeds the same device. Nothing here blocks.
#[derive(Clone)]
pub struct CameraFeed {
    commands: mpsc::UnboundedSender<CameraCommand>,
    samples: mpsc::Sender<Sample>,
    /// Whether samples are being dropped until a keyframe.
    gap: Arc<AtomicBool>,
}

impl CameraFeed {
    pub(super) fn new() -> (Self, CameraQueues) {
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let (samples_tx, samples) = mpsc::channel(SAMPLE_QUEUE);
        let feed = Self { commands: commands_tx, samples: samples_tx, gap: Arc::new(AtomicBool::new(false)) };
        (feed, CameraQueues { commands, samples })
    }

    /// Offer the host a camera producing H.264 in `format`. It is announced as soon as the
    /// host has opened the enumeration channel and agreed a version, so plugging before
    /// then is fine; plugging again in another format replaces the device.
    pub fn plug(&self, format: Format) {
        // A closed queue is a session that has ended, and the device went with it.
        let _ = self.commands.send(CameraCommand::Plug(format));
    }

    /// Withdraw the device; the host sees the camera unplug.
    pub fn unplug(&self) {
        let _ = self.commands.send(CameraCommand::Unplug);
    }

    /// One encoded H.264 picture, Annex B, parameter sets inline on a keyframe.
    ///
    /// The session sends it while the host is streaming and drops it otherwise. What is
    /// decided here is only whether it reaches the session: a full queue drops it and
    /// every later sample but a keyframe, and says so once, with [`Fed::Dropped`].
    pub fn sample(&self, data: &[u8], keyframe: bool) -> Fed {
        if !keyframe && self.gap.load(Ordering::Relaxed) {
            return Fed::Skipped;
        }
        match self.samples.try_send(Sample { data: data.to_vec(), keyframe }) {
            Ok(()) => {
                self.gap.store(false, Ordering::Relaxed);
                Fed::Queued
            }
            // A keyframe lost to the queue is owed again, or the gap never closes.
            Err(TrySendError::Full(_)) if keyframe || !self.gap.swap(true, Ordering::Relaxed) => {
                self.gap.store(true, Ordering::Relaxed);
                Fed::Dropped
            }
            Err(TrySendError::Full(_)) => Fed::Skipped,
            Err(TrySendError::Closed(_)) => Fed::Ended,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VGA: Format = Format { width: 640, height: 480, fps_numerator: 30, fps_denominator: 1 };

    /// A full queue opens a gap, reported once; deltas inside it are skipped; a keyframe
    /// that finds room closes it, and one that does not is owed again.
    #[test]
    fn a_full_queue_drops_to_a_keyframe_and_says_so_once() {
        let (feed, mut queues) = CameraFeed::new();
        for _ in 0..SAMPLE_QUEUE {
            assert_eq!(feed.sample(&[1], false), Fed::Queued);
        }
        assert_eq!(feed.sample(&[2], false), Fed::Dropped);
        assert_eq!(feed.sample(&[3], false), Fed::Skipped);
        assert_eq!(feed.sample(&[4], true), Fed::Dropped, "a keyframe lost to the queue is owed again");
        assert!(queues.samples.try_recv().is_ok());
        assert_eq!(feed.sample(&[5], false), Fed::Skipped, "room is not a keyframe");
        assert_eq!(feed.sample(&[6], true), Fed::Queued);
        assert!(queues.samples.try_recv().is_ok());
        assert_eq!(feed.sample(&[7], false), Fed::Queued, "the gap closed");
    }

    /// The device's commands are never lost behind samples, and go first.
    #[tokio::test]
    async fn commands_go_before_samples_and_the_end_is_reported() {
        let (feed, mut queues) = CameraFeed::new();
        assert_eq!(feed.sample(&[1], true), Fed::Queued);
        feed.plug(VGA);
        assert!(matches!(queues.next().await, Some(CameraInput::Command(CameraCommand::Plug(VGA)))));
        assert!(matches!(queues.next().await, Some(CameraInput::Sample(Sample { keyframe: true, .. }))));
        drop(feed);
        assert!(queues.next().await.is_none());

        let (feed, queues) = CameraFeed::new();
        drop(queues);
        assert_eq!(feed.sample(&[1], true), Fed::Ended);
    }
}
