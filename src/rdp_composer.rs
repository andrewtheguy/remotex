//! An RDP host's graphics pipeline composed here, for a session started with
//! Motion ([`crate::config::Choices::motion`]).
//!
//! Every other session passes the pipeline to the page, which composes it. This
//! one composes it with the same compositor, `remotex_rdp_graphics`, on a thread
//! of its own: a desktop drawn losslessly arrives as planar, a couple of megabytes
//! a frame, and decoding it on the engine's task would hold up the input behind
//! it. The engine reads the picture out of the [`Framebuffer`] the thread
//! composes into, as it reads a bitmap update's, and is told what each run painted
//! in the order the runs came.

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use anyhow::{Context as _, Result};
use tokio::sync::mpsc;

use crate::rdp_client::{Composed, Compositor, Framebuffer};

/// What the thread is handed, in order.
enum Job {
    /// A pipeline begins: compose from nothing.
    Start,
    /// A run of commands, and the frame it ends, if any.
    Run { commands: Vec<u8>, frame: Option<u32> },
}

/// One run composed: what it painted, and the frame it ended, which the host is
/// owed an acknowledgement for.
pub struct Ran {
    pub composed: Composed,
    pub frame: Option<u32>,
}

/// The composing thread, and the picture it composes into.
pub struct Composer {
    jobs: std_mpsc::Sender<Job>,
    ran: mpsc::UnboundedReceiver<Result<Ran>>,
    framebuffer: Arc<Framebuffer>,
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
        Ok(Self { jobs, ran, framebuffer })
    }

    /// The picture as composed so far, which the thread goes on composing into.
    pub fn framebuffer(&self) -> Arc<Framebuffer> {
        Arc::clone(&self.framebuffer)
    }

    /// A pipeline begins, from nothing: the host opened its channel, or opened
    /// it again.
    pub fn begin(&self) -> Result<()> {
        self.jobs.send(Job::Start).context("the RDP composer stopped")
    }

    /// Queue a run of the pipeline's commands.
    pub fn run(&self, commands: Vec<u8>, frame: Option<u32>) -> Result<()> {
        self.jobs.send(Job::Run { commands, frame }).context("the RDP composer stopped")
    }

    /// The next run composed, in the order they were queued. An error is a
    /// command that did not decode: the compositor no longer holds what the host
    /// believes its client does, and nothing composed after it can be trusted.
    pub async fn ran(&mut self) -> Result<Ran> {
        self.ran.recv().await.context("the RDP composer stopped")?
    }
}

fn compose(queue: std_mpsc::Receiver<Job>, done: mpsc::UnboundedSender<Result<Ran>>, framebuffer: Arc<Framebuffer>) {
    let mut compositor = Compositor::sharing(Arc::clone(&framebuffer));
    for job in queue {
        match job {
            Job::Start => compositor = Compositor::sharing(Arc::clone(&framebuffer)),
            Job::Run { commands, frame } => {
                let ran = compositor
                    .compose(&commands)
                    .map(|composed| Ran { composed, frame })
                    .context("composing the host's graphics pipeline");
                let failed = ran.is_err();
                if done.send(ran).is_err() || failed {
                    return;
                }
            }
        }
    }
}
