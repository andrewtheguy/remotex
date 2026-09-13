//! The microphone side of a [`Session`](super::Session): a feed the caller hands PCM to,
//! and a sink the session tells the host's decisions to.
//!
//! The protocol is [`rdpeai`](super::proto::rdpeai), run on the session thread beside
//! everything else on the connection. PCM waits for that thread in a bounded queue, and a
//! buffer that finds the queue full is dropped here: a microphone late by a queue's worth
//! is worse than one missing a moment, and audio, unlike H.264, has nothing to resume.

use tokio::sync::mpsc::{self, error::TrySendError};

use super::proto::rdpeai::Format;

/// How many buffers may wait for the session thread: at the gateway's 20 ms groups, a
/// third of a second.
const SAMPLE_QUEUE: usize = 16;

/// Where the host's microphone decisions go.
///
/// Called on the session's thread, in step with the desktop being decoded, so nothing here
/// may wait.
pub trait MicrophoneSink: Send {
    /// The host agreed a protocol version, so microphone redirection is on offer. A
    /// Windows host opens the channel only once something on it starts recording.
    fn negotiated(&self, version: u32);
    /// An application on the host started recording: PCM in `format` is wanted.
    fn opened(&self, format: Format);
    /// The recording ended.
    fn closed(&self);
}

/// The microphone side of a [`Session`](super::Session), for a session configured with
/// [`Connect::microphone`](super::Connect::microphone).
///
/// Cheap to clone, and every clone feeds the same recording. Nothing here blocks.
#[derive(Clone)]
pub struct MicrophoneFeed {
    samples: mpsc::Sender<Vec<u8>>,
}

impl MicrophoneFeed {
    pub(super) fn new() -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (samples, queue) = mpsc::channel(SAMPLE_QUEUE);
        (Self { samples }, queue)
    }

    /// Interleaved 16-bit little-endian PCM in the format the host opened. The session
    /// sends it while the host records and drops it otherwise. Returns whether it reached
    /// the session: `false` for a full queue or an ended session.
    pub fn sample(&self, pcm: Vec<u8>) -> bool {
        match self.samples.try_send(pcm) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Closed(_)) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_drops_rather_than_waits() {
        let (feed, mut queue) = MicrophoneFeed::new();
        for _ in 0..SAMPLE_QUEUE {
            assert!(feed.sample(vec![0; 2]));
        }
        assert!(!feed.sample(vec![1; 2]));
        assert_eq!(queue.try_recv().unwrap(), vec![0; 2]);
        assert!(feed.sample(vec![2; 2]));
        drop(queue);
        assert!(!feed.sample(vec![3; 2]), "an ended session takes nothing");
    }
}
