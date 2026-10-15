//! An RDP host's graphics pipeline composed here, for a session started with
//! Motion ([`crate::config::Choices::motion`]).
//!
//! Every other session passes the pipeline to the page, which composes it. This
//! one composes it with the same compositor, `remotex_rdp_graphics`, on a thread
//! of its own: a desktop drawn losslessly arrives as planar, a couple of megabytes
//! a frame, and decoding it on the engine's task would hold up the input behind
//! it. The engine reads the picture out of the [`Framebuffer`] the thread
//! composes into, as it reads a bitmap update's, and is told what each run painted
//! in the order the runs came. The thread stops at the end of each frame until the
//! engine has let go of what it was told of it, so a frame is read as it was
//! composed and never with the next one half drawn over it.

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use anyhow::{Context as _, Result};
use tokio::sync::mpsc;

use crate::rdp_client::{Composed, Compositor, Framebuffer};

/// What the thread is handed, in order.
enum Job {
    /// A pipeline begins: compose from nothing.
    Start,
    /// A run of commands, the frame it ends, if any, and the generation it was
    /// queued in.
    Run { commands: Vec<u8>, frame: Option<u32>, generation: u64 },
}

/// One run composed: what it painted, and the frame it ended, which the host is
/// owed an acknowledgement for. A run that ended a frame holds the thread until
/// this is dropped, so the framebuffer is that frame's while it is held.
pub struct Ran {
    pub composed: Composed,
    pub frame: Option<u32>,
    /// Queued before the picture last started over, on a resize or a restart:
    /// what it painted belongs to a picture that is gone, though its frame is
    /// still owed its acknowledgement.
    pub stale: bool,
    _hold: Option<std_mpsc::Sender<()>>,
}

/// The composing thread, and the picture it composes into.
pub struct Composer {
    jobs: std_mpsc::Sender<Job>,
    ran: mpsc::UnboundedReceiver<Result<(Ran, u64)>>,
    framebuffer: Arc<Framebuffer>,
    generation: u64,
}

impl Composer {
    pub fn start() -> Result<Self> {
        let framebuffer = Arc::new(Framebuffer::new());
        let (jobs, queue) = std_mpsc::channel();
        let (done, ran) = mpsc::unbounded_channel();
        let shared = Arc::clone(&framebuffer);
        std::thread::Builder::new()
            .name("rdp-composer".to_owned())
            .spawn(move || compose(queue, done, shared))
            .context("starting the RDP composer's thread")?;
        Ok(Self { jobs, ran, framebuffer, generation: 0 })
    }

    /// The picture as composed so far, which the thread goes on composing into.
    pub fn framebuffer(&self) -> Arc<Framebuffer> {
        Arc::clone(&self.framebuffer)
    }

    /// A pipeline begins, from nothing: the host opened its channel, or opened
    /// it again. What was queued before it is stale.
    pub fn begin(&mut self) -> Result<()> {
        self.fence();
        self.jobs.send(Job::Start).context("the RDP composer stopped")
    }

    /// The picture starts over, on a resize: what was queued before it is
    /// stale.
    pub fn fence(&mut self) {
        self.generation += 1;
    }

    /// Queue a run of the pipeline's commands.
    pub fn run(&self, commands: Vec<u8>, frame: Option<u32>) -> Result<()> {
        let generation = self.generation;
        self.jobs.send(Job::Run { commands, frame, generation }).context("the RDP composer stopped")
    }

    /// The next run composed, in the order they were queued. An error is a
    /// command that did not decode: the compositor no longer holds what the host
    /// believes its client does, and nothing composed after it can be trusted.
    pub async fn ran(&mut self) -> Result<Ran> {
        let (mut ran, generation) = self.ran.recv().await.context("the RDP composer stopped")??;
        ran.stale = generation != self.generation;
        Ok(ran)
    }
}

fn compose(
    queue: std_mpsc::Receiver<Job>,
    done: mpsc::UnboundedSender<Result<(Ran, u64)>>,
    framebuffer: Arc<Framebuffer>,
) {
    let mut compositor = Compositor::sharing(Arc::clone(&framebuffer));
    for job in queue {
        match job {
            Job::Start => compositor = Compositor::sharing(Arc::clone(&framebuffer)),
            Job::Run { commands, frame, generation } => {
                let (hold, held) = std_mpsc::channel();
                let ran = compositor
                    .compose(&commands)
                    .map(|composed| {
                        let hold = (composed.frames > 0).then_some(hold);
                        (Ran { composed, frame, stale: false, _hold: hold }, generation)
                    })
                    .context("composing the host's graphics pipeline");
                let failed = ran.is_err();
                if done.send(ran).is_err() || failed {
                    return;
                }
                // Until the engine lets go of a frame's end: no sender is ever
                // sent on, so this returns once the last one is dropped.
                let _ = held.recv();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// One frame, start to end, with nothing drawn in it.
    fn frame(frame: u32) -> Vec<u8> {
        let pdu = |id: u16, body: &[u8]| {
            let mut pdu = id.to_le_bytes().to_vec();
            pdu.extend(0u16.to_le_bytes());
            pdu.extend((8 + body.len() as u32).to_le_bytes());
            pdu.extend(body);
            pdu
        };
        let mut start = 0u32.to_le_bytes().to_vec();
        start.extend(frame.to_le_bytes());
        let mut commands = pdu(0x000B, &start);
        commands.extend(pdu(0x000C, &frame.to_le_bytes()));
        commands
    }

    #[tokio::test]
    async fn a_frame_is_held_until_the_engine_lets_go_of_it() {
        let mut composer = Composer::start().unwrap();
        composer.run(frame(7), Some(7)).unwrap();
        composer.run(frame(8), Some(8)).unwrap();
        let first = composer.ran().await.unwrap();
        assert_eq!((first.frame, first.composed.frames), (Some(7), 1));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), composer.ran()).await.is_err(),
            "the next frame is not composed over one still being read"
        );
        drop(first);
        assert_eq!(composer.ran().await.unwrap().frame, Some(8));
    }

    #[tokio::test]
    async fn what_was_queued_before_a_fence_is_stale() {
        let mut composer = Composer::start().unwrap();
        composer.run(frame(7), Some(7)).unwrap();
        composer.fence();
        composer.run(frame(8), Some(8)).unwrap();
        let before = composer.ran().await.unwrap();
        assert!(before.stale && before.frame == Some(7), "still acknowledged, but not painted");
        drop(before);
        assert!(!composer.ran().await.unwrap().stale);
        composer.begin().unwrap();
        composer.run(frame(9), Some(9)).unwrap();
        assert!(!composer.ran().await.unwrap().stale, "a restart fences what came before it, not after");
    }
}
