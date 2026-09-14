//! The microphone side of a [`Session`](super::Session): a feed the caller hands PCM to,
//! and a sink the session tells the host's decisions to.
//!
//! The protocol is [`rdpeai`](super::proto::rdpeai), run on the session thread beside
//! everything else on the connection. PCM waits for that thread in a bounded queue that
//! keeps the newest audio: a buffer arriving to a full queue pushes out the oldest, so a
//! thread that was held up resumes at the live edge rather than a queue behind it. A
//! microphone late is worse than one missing a moment, and audio, unlike H.264, has
//! nothing to resume. [`MicrophoneFeed::flush`] empties the queue ahead of anything in it,
//! for audio that must not reach the host at all.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

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

/// One thing the caller's feed wants done, as the session thread takes it.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MicrophoneInput {
    Pcm(Vec<u8>),
    /// Everything queued before this was dropped, and a partial packet goes with it.
    Flush,
}

struct Shared {
    state: Mutex<State>,
    ready: Notify,
}

#[derive(Default)]
struct State {
    buffers: VecDeque<Vec<u8>>,
    flush: bool,
    /// The session thread is gone.
    ended: bool,
}

/// The session thread's end of a [`MicrophoneFeed`].
pub(super) struct MicrophoneQueue {
    shared: Arc<Shared>,
}

impl MicrophoneQueue {
    /// The next thing the caller wants, a flush before any buffer.
    pub(super) async fn next(&mut self) -> MicrophoneInput {
        loop {
            {
                let mut state = self.shared.state.lock().expect("microphone queue lock");
                if std::mem::take(&mut state.flush) {
                    return MicrophoneInput::Flush;
                }
                if let Some(pcm) = state.buffers.pop_front() {
                    return MicrophoneInput::Pcm(pcm);
                }
            }
            // A notification sent between the check and here is kept as a permit.
            self.shared.ready.notified().await;
        }
    }
}

impl Drop for MicrophoneQueue {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("microphone queue lock");
        state.ended = true;
        state.buffers.clear();
    }
}

/// The microphone side of a [`Session`](super::Session), for a session configured with
/// [`Connect::microphone`](super::Connect::microphone).
///
/// Cheap to clone, and every clone feeds the same recording. Nothing here blocks.
#[derive(Clone)]
pub struct MicrophoneFeed {
    shared: Arc<Shared>,
}

impl MicrophoneFeed {
    pub(super) fn new() -> (Self, MicrophoneQueue) {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), ready: Notify::new() });
        (Self { shared: Arc::clone(&shared) }, MicrophoneQueue { shared })
    }

    /// Interleaved 16-bit little-endian PCM in the format the host opened. The session
    /// sends it while the host records and drops it otherwise. A full queue drops its
    /// oldest buffer to take this one. Returns `false` once the session has ended.
    pub fn sample(&self, pcm: Vec<u8>) -> bool {
        {
            let mut state = self.shared.state.lock().expect("microphone queue lock");
            if state.ended {
                return false;
            }
            if state.buffers.len() == SAMPLE_QUEUE {
                state.buffers.pop_front();
            }
            state.buffers.push_back(pcm);
        }
        self.shared.ready.notify_one();
        true
    }

    /// Drop every buffer not yet taken, and the partial packet the session holds: the
    /// audio that follows, if any, is a different stream.
    pub fn flush(&self) {
        {
            let mut state = self.shared.state.lock().expect("microphone queue lock");
            if state.ended {
                return;
            }
            state.buffers.clear();
            state.flush = true;
        }
        self.shared.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_queue_keeps_the_newest_audio() {
        let (feed, mut queue) = MicrophoneFeed::new();
        for n in 0..=SAMPLE_QUEUE {
            assert!(feed.sample(vec![n as u8]));
        }
        assert_eq!(queue.next().await, MicrophoneInput::Pcm(vec![1]), "the oldest was dropped");
        drop(queue);
        assert!(!feed.sample(vec![0]), "an ended session takes nothing");
    }

    #[tokio::test]
    async fn a_flush_goes_before_and_instead_of_what_was_queued() {
        let (feed, mut queue) = MicrophoneFeed::new();
        feed.sample(vec![1]);
        feed.sample(vec![2]);
        feed.flush();
        feed.sample(vec![3]);
        assert_eq!(queue.next().await, MicrophoneInput::Flush);
        assert_eq!(queue.next().await, MicrophoneInput::Pcm(vec![3]));
    }

    #[tokio::test]
    async fn a_waiting_session_wakes_for_audio() {
        let (feed, mut queue) = MicrophoneFeed::new();
        let waiting = tokio::spawn(async move { queue.next().await });
        tokio::task::yield_now().await;
        feed.sample(vec![7]);
        assert_eq!(waiting.await.unwrap(), MicrophoneInput::Pcm(vec![7]));
    }
}
