//! The session: its configuration, its thread, and the loop that drives it.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use log::{debug, info, warn};
use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use tokio::time::Duration;

use super::connect::{self, Connected};
use super::error::Error;
use super::framebuffer::{Framebuffer, Rect};
use super::input::{Command, Input};
use super::pointer::Cursor;
use super::proto::bitmap::MAX_DESKTOP_BYTES;
use super::proto::capabilities::DemandActive;
use super::proto::fastpath::{self, Fragments, Update};
use super::proto::frame::Frames;
use super::proto::pointer::{self, Pointer};
use super::proto::share::{self, Pdu};
use super::proto::{bitmap, channel, desktop, display, dvc, input, mcs, tls};

// ------------------------------------------------------------------ configuration

/// Everything needed to open a session.
pub struct Connect {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    /// The desktop size to ask for. The server may answer with something else,
    /// which arrives as [`Event::Connected`] and, later, as [`Event::Resize`].
    pub width: u32,
    pub height: u32,
    /// Whether to open Display Control, which is what makes
    /// [`Input::resize`] do anything.
    ///
    /// A server answers a monitor layout with a Deactivation-Reactivation Sequence:
    /// it tears the desktop and the capability set down and builds them again, after
    /// which it renders the new size from scratch. This client sees one
    /// [`Event::Resize`] at the end of it.
    pub resize: bool,
}

// ------------------------------------------------------------------ events

/// Something the session did.
#[derive(Clone, Debug)]
pub enum Event {
    /// The desktop exists and its size is settled. Always the first event of a
    /// successful session; a failed one goes straight to [`Event::Ended`].
    Connected { width: u32, height: u32 },
    /// This rectangle of the framebuffer changed.
    ///
    /// Bitmap updates carry no frame boundary, so nothing here says where one
    /// picture ends and the next begins; a consumer that needs to present coherent
    /// frames paces them itself.
    Paint(Rect),
    /// The desktop was redefined — resized, or rebuilt at the same size — and the
    /// framebuffer has already been resized and cleared, so everything is about to
    /// be repainted.
    ///
    /// Sent whether the change was asked for or not: a server may resize a session
    /// on its own, and that arrives here identically to the answer to an
    /// [`Input::resize`]. A server normally repaints afterwards but is not obliged
    /// to, and the framebuffer is blank until it does; a caller that cannot show a
    /// blank desktop should follow this with [`Input::refresh`].
    Resize { width: u32, height: u32 },
    /// The server offered Display Control, so [`Input::resize`] now has somewhere
    /// to go. Only ever sent on a session configured with [`Connect::resize`], and
    /// not at all by a server that does not implement MS-RDPEDISP.
    ///
    /// It is **not** a promise that the next resize will be honoured: a Windows host
    /// sends this and then ignores layouts for several seconds more, silently — see
    /// [`Input::resize`].
    ///
    /// `max_area` is the largest total monitor area the server will accept, in
    /// pixels; this client asks for one monitor, so it bounds `width * height`.
    ResizeReady { max_area: u64 },
    Cursor(Cursor),
    /// The session is over, and the channel is about to close. `Ok(())` is an
    /// orderly disconnection from either side.
    Ended(Result<(), Error>),
}

/// How many events may wait for the caller before the session thread waits for it.
///
/// Bounded so a caller that falls behind slows the session rather than growing a
/// queue: paint rectangles fold together on the session thread while the queue is
/// full (see `Active::paint`), and anything else waits for room — which is the
/// socket going unread, and so the server slowing down.
const EVENT_QUEUE: usize = 64;

/// How long one write to the host may take before the connection is given up.
///
/// A peer that stops reading leaves a write pending forever where nothing else
/// notices — Linux's `TCP_USER_TIMEOUT` bounds it, macOS and Windows have no
/// equivalent — and the session thread cannot see its queue, shutdown included,
/// while it waits. The same 30 seconds `engine` gives unacknowledged data.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long dropping a [`Session`] waits for its thread before leaving it behind.
///
/// Enough for the disconnect the thread sends on the way out (itself bounded to a
/// second), and short enough that a thread stuck somewhere else cannot hold the
/// caller's thread with it. A thread left behind still ends — at the latest when
/// its write times out or its socket fails — and owns nothing the caller needs.
const JOIN_BUDGET: Duration = Duration::from_secs(3);

// ------------------------------------------------------------------ the session handle

/// A live RDP session.
///
/// Dropping this asks the session thread to disconnect and waits for it, so a
/// `Session` that has gone out of scope has really stopped — no detached thread
/// still holding a socket open, and no session on the server claimed by a client
/// nobody is watching.
pub struct Session {
    input: Input,
    framebuffer: Arc<Framebuffer>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Closed by the thread as it finishes, which is what lets `drop` wait for it
    /// with a deadline — a `JoinHandle` has none.
    finished: std::sync::mpsc::Receiver<()>,
}

impl Session {
    /// Connect, on a thread of its own.
    ///
    /// Returns immediately: the connection happens on the new thread and its outcome
    /// arrives as the first [`Event`] — [`Event::Connected`] or [`Event::Ended`].
    /// Connecting takes seconds (TCP, TLS, CredSSP, licensing, the first desktop),
    /// and a `start` that blocked for them would have to be called from a thread the
    /// caller was willing to lose anyway.
    pub fn start(config: Connect) -> (Self, mpsc::Receiver<Event>) {
        install_crypto_provider();
        let (events, receiver) = mpsc::channel(EVENT_QUEUE);
        let (finished_tx, finished) = std::sync::mpsc::channel::<()>();
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let input = Input::new(commands_tx);
        let framebuffer = Arc::new(Framebuffer::new());

        let spawned = std::thread::Builder::new().name("rdp".into()).spawn({
            let framebuffer = Arc::clone(&framebuffer);
            let events = events.clone();
            let stop = input.stopped();
            move || {
                // Dropped when this closure returns, however it returns.
                let _finished = finished_tx;
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
                let runtime = match runtime {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        // Nothing has been sent yet, so there is room for this.
                        let _ = events.try_send(Event::Ended(Err(Error::new(format!(
                            "could not start the RDP session runtime: {e}"
                        )))));
                        return;
                    }
                };
                // A panic here would otherwise take the thread down with no `Ended`
                // event, and the caller would wait on the receiver forever. Converted
                // into the disconnection it really is.
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    runtime.block_on(thread_main(config, commands, &framebuffer, &events, stop))
                }));
                let result = outcome
                    .unwrap_or_else(|_| Err(Error::new("the RDP session thread panicked")));
                // The last word waits for room like any other event, but not past the
                // point where a caller dropping this session has given up on the
                // thread: staying longer would hold a thread nobody is waiting for
                // against a queue nobody is draining.
                runtime.block_on(async {
                    if tokio::time::timeout(JOIN_BUDGET, events.send(Event::Ended(result)))
                        .await
                        .is_err()
                    {
                        warn!(
                            "rdp: the caller took no event for {}s, so the session's last one \
                             is dropped",
                            JOIN_BUDGET.as_secs()
                        );
                    }
                });
            }
        });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(e) => {
                // The closure never ran, so nothing else will end this session. The
                // queue is empty, so this cannot be refused for room.
                let _ = events.try_send(Event::Ended(Err(Error::new(format!(
                    "could not start the RDP session thread: {e}"
                )))));
                None
            }
        };
        (Self { input, framebuffer, thread, finished }, receiver)
    }

    /// Keyboard, mouse, refresh and resize.
    pub fn input(&self) -> &Input {
        &self.input
    }

    /// The framebuffer, kept up to date by the session thread.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.input.shutdown();
        if let Some(thread) = self.thread.take() {
            // Joined rather than detached whenever it can be: the thread's loop — and
            // its connect, which races the same queue — wakes on the command, sends
            // its disconnect, and returns. Bounded, because a thread busy elsewhere —
            // in a write the host is not reading, or waiting for room in a queue
            // nobody is draining — would otherwise hold this thread too.
            match self.finished.recv_timeout(JOIN_BUDGET) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    warn!(
                        "rdp: the session thread did not stop within {}s; leaving it to finish \
                         on its own",
                        JOIN_BUDGET.as_secs()
                    );
                }
                _ => {
                    let _ = thread.join();
                }
            }
        }
    }
}

/// rustls needs a process-wide crypto provider before the first TLS handshake,
/// and `ring` is the one in the tree. Installed here rather than in `main`, so
/// every path that opens a session — the gateway, a test, a probe — has it.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // An error means some other code in the process installed one first,
        // which is just as good.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

async fn thread_main(
    config: Connect,
    mut commands: mpsc::UnboundedReceiver<Command>,
    framebuffer: &Framebuffer,
    events: &mpsc::Sender<Event>,
    stop: watch::Receiver<bool>,
) -> Result<(), Error> {
    run(config, &mut commands, framebuffer, events, stop).await.map_err(Error::from)
}

/// The same, in the errors the protocol modules raise. They become the session's one
/// sentence at the boundary above, where a caller sees them.
async fn run(
    config: Connect,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    framebuffer: &Framebuffer,
    events: &mpsc::Sender<Event>,
    stop: watch::Receiver<bool>,
) -> Result<()> {
    let connected = tokio::select! {
        // Biased so a session dropped mid-connect stops at the next await rather
        // than finishing a handshake nobody is waiting for.
        biased;
        () = shutdown_requested(commands) => {
            bail!("the session was ended before it connected");
        }
        connected = connect::connect(&config) => connected?,
    };
    let (width, height) = (u32::from(connected.demand.width), u32::from(connected.demand.height));
    info!("rdp: connected, desktop {width}x{height}");
    // Before the framebuffer is sized to it.
    affordable(width, height)?;
    framebuffer.resize(width, height);
    // The first event of the session, so there is room for it.
    let _ = events.send(Event::Connected { width, height }).await;
    Active::new(connected, framebuffer, events, stop).run(commands).await
}

/// Resolves once the caller has asked this session to stop, or dropped every
/// handle that could. Anything else queued before the desktop exists is dropped:
/// there is nothing yet for input to act on.
async fn shutdown_requested(commands: &mut mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = commands.recv().await {
        if matches!(command, Command::Shutdown) {
            return;
        }
    }
}

// ------------------------------------------------------------------ the active session

/// Most rectangles the waiting paint holds before it collapses to one bounding box.
const DAMAGE_CAP: usize = 32;

/// How many queued commands one turn of the loop takes before it goes back to the
/// socket: enough to fill a fast-path input PDU, few enough that a burst of input
/// cannot starve the desktop.
const COMMANDS_PER_TURN: usize = input::MAX_EVENTS;

/// What the server said about the share this client is looking at.
///
/// All of it is replaced wholesale when the server rebuilds the desktop, which is
/// why it is one struct: after a Deactivation-Reactivation Sequence the share
/// identifier, the size and the limits are all the new share's.
struct Share {
    /// Names the share, and every data PDU either side sends carries it.
    id: u32,
    width: u32,
    height: u32,
    /// The largest chunk a virtual channel PDU may be split into.
    chunk: usize,
    /// Which of the two ways of asking for a repaint this server reads — see
    /// [`desktop::repaint`].
    refresh_rect: bool,
    suppress_output: bool,
}

impl From<&DemandActive> for Share {
    fn from(demand: &DemandActive) -> Self {
        Self {
            id: demand.share_id,
            width: u32::from(demand.width),
            height: u32::from(demand.height),
            chunk: demand.chunk,
            refresh_rect: demand.refresh_rect,
            suppress_output: demand.suppress_output,
        }
    }
}

struct Active<'a> {
    frames: Frames<ReadHalf<tls::Stream>>,
    writer: WriteHalf<tls::Stream>,
    /// The frame being read, kept across turns so the buffer is allocated once.
    frame: Vec<u8>,
    /// This client's own MCS user, which every PDU it sends names as its source.
    user: u16,
    io_channel: u16,
    /// The static virtual channel dynamic channels are opened over, for a session
    /// that asked to be resizable.
    dynamic: Option<u16>,
    share: Share,

    /// The pieces of a fast-path update that arrived cut up.
    fragments: Fragments,
    scratch: bitmap::Scratch,
    /// One decoded rectangle, reused: every bitmap update fills it and empties it.
    pixels: Vec<u8>,
    cursors: pointer::Cache,

    /// The chunks of a virtual channel PDU, and the dynamic channel PDUs inside
    /// them. Both outlive a reactivation: the channel is the connection's, not the
    /// share's.
    chunks: channel::Reassembly,
    incoming: dvc::Incoming,
    /// The number the server's Create Request gave Display Control, once it has
    /// opened it.
    control: Option<u32>,
    /// What Display Control said it would lay out, which arrives after the channel
    /// is open and before a layout may be sent.
    caps: Option<display::Capabilities>,
    /// Whether [`Event::ResizeReady`] has gone out.
    resize_ready: bool,
    /// The most recent size asked for before the channel was ready — only the most
    /// recent, since a resize supersedes every earlier one rather than queueing
    /// behind it.
    pending_resize: Option<(u32, u32, u32)>,

    framebuffer: &'a Framebuffer,
    events: &'a mpsc::Sender<Event>,
    /// Painted rectangles not yet handed to the caller, folded together while the
    /// event queue is full — see [`EVENT_QUEUE`].
    damage: Vec<Rect>,
    /// Raised once the session has been asked to stop, which is what lets a wait
    /// for room in the caller's queue end — see [`Self::deliver`].
    stop: watch::Receiver<bool>,
}

impl<'a> Active<'a> {
    fn new(
        connected: Connected,
        framebuffer: &'a Framebuffer,
        events: &'a mpsc::Sender<Event>,
        stop: watch::Receiver<bool>,
    ) -> Self {
        let Connected { frames, writer, user, io_channel, dynamic, demand } = connected;
        Self {
            frames,
            writer,
            frame: Vec::new(),
            user,
            io_channel,
            dynamic,
            share: Share::from(&demand),
            fragments: Fragments::new(demand.multifragment),
            scratch: bitmap::Scratch::default(),
            pixels: Vec::new(),
            cursors: pointer::Cache::new(),
            chunks: channel::Reassembly::new(),
            incoming: dvc::Incoming::new(),
            control: None,
            caps: None,
            resize_ready: false,
            pending_resize: None,
            framebuffer,
            events,
            damage: Vec::new(),
            stop,
        }
    }

    async fn run(mut self, commands: &mut mpsc::UnboundedReceiver<Command>) -> Result<()> {
        // Updates that arrived while the share was being finalized were read past
        // there — the server may start painting once it has the Font List, which is
        // before this client has the Font Map that ends the sequence — so the desktop
        // is asked for again here, the way it is after a reactivation.
        self.refresh().await?;
        loop {
            tokio::select! {
                read = self.frames.next(&mut self.frame) => {
                    read?;
                    if let Some(ended) = self.on_frame().await? {
                        return ended;
                    }
                }
                // Paint that waited for room in the queue, sent as soon as there is
                // some even if the host goes quiet.
                permit = self.events.reserve(), if !self.damage.is_empty() => {
                    match permit {
                        Ok(permit) => {
                            permit.send(Event::Paint(self.damage.remove(0)));
                            self.try_send_damage();
                        }
                        // The caller stopped listening; nothing will read these.
                        Err(_) => self.damage.clear(),
                    }
                }
                command = commands.recv() => {
                    let stop = match command {
                        Some(command) => self.on_commands(command, commands).await?,
                        // Every handle is gone, which is a shutdown nobody sent.
                        None => true,
                    };
                    if stop {
                        self.disconnect().await;
                        return Ok(());
                    }
                }
            }
        }
    }

    /// One frame off the wire. `Some` is the session's end.
    ///
    /// The frame is taken out of `self` for the duration and put back afterwards, so
    /// that reading it and acting on it — which is most of this file — do not both
    /// need the whole session at once.
    async fn on_frame(&mut self) -> Result<Option<Result<()>>> {
        let frame = std::mem::take(&mut self.frame);
        let outcome = self.dispatch(&frame).await;
        self.frame = frame;
        outcome
    }

    async fn dispatch(&mut self, frame: &[u8]) -> Result<Option<Result<()>>> {
        // A frame with no first byte is not one [`Frames`] hands out.
        if fastpath::is_output(frame[0]) {
            self.on_updates(frame).await?;
            return Ok(None);
        }
        match mcs::send_data_indication(frame)? {
            mcs::Indication::Disconnect(reason) => {
                info!("rdp: the host left the conference: {reason}");
                Ok(Some(match reason.is_orderly() {
                    true => Ok(()),
                    false => Err(anyhow!("the host ended the session: {reason}")),
                }))
            }
            mcs::Indication::Data(data) if Some(data.channel) == self.dynamic => {
                self.on_dynamic(data.payload).await?;
                Ok(None)
            }
            mcs::Indication::Data(data) if data.channel == self.io_channel => {
                self.on_share(data.payload).await
            }
            // A channel this client neither asked for nor joined. A server does not
            // send one, and a PDU on one is nothing this session can act on.
            mcs::Indication::Data(data) => {
                debug!("rdp: ignoring {} bytes on channel {}", data.payload.len(), data.channel);
                Ok(None)
            }
        }
    }

    /// The fast path: everything the server draws.
    async fn on_updates(&mut self, frame: &[u8]) -> Result<()> {
        let mut painted = Vec::new();
        for piece in fastpath::updates(frame)? {
            let piece = piece?;
            let cursor = {
                // Disjoint pieces of the session, so that an update may borrow the
                // reassembler it came out of while the decoders it feeds are used.
                let Self { fragments, scratch, pixels, cursors, framebuffer, share, .. } = self;
                let Some(update) = fragments.push(piece)? else {
                    continue;
                };
                draw(update, scratch, pixels, cursors, framebuffer, share, &mut painted)?
            };
            for rect in painted.drain(..) {
                self.paint(rect);
            }
            if let Some(cursor) = cursor {
                self.send(Event::Cursor(cursor)).await;
            }
        }
        Ok(())
    }

    /// The slow path on the I/O channel: everything that is not a picture.
    async fn on_share(&mut self, payload: &[u8]) -> Result<Option<Result<()>>> {
        match share::decode(payload)? {
            Pdu::DeactivateAll => {
                let stopped = self.reactivate().await?;
                if stopped {
                    // Asked to stop mid-sequence: leave as politely as the loop
                    // itself does, rather than dropping the connection.
                    self.disconnect().await;
                    return Ok(Some(Ok(())));
                }
                Ok(None)
            }
            // A share this client did not ask to be rebuilt: the server sends the
            // Deactivate All first, and `reactivate` reads the Demand Active.
            Pdu::DemandActive(_) => {
                bail!("the host demanded a share without deactivating the last one")
            }
            Pdu::Data(data) if data.kind == share::SET_ERROR_INFO => {
                match desktop::error_info(data.body) {
                    Ok(()) => Ok(None),
                    Err(reported) => {
                        info!("rdp: {reported}");
                        Ok(Some(Err(reported.into())))
                    }
                }
            }
            // Who logged on, how the server's own monitors are arranged, what it
            // measured the link at, what it would like the keyboard lights to do:
            // nothing here acts on any of them.
            Pdu::Data(data) => {
                debug!("rdp: ignoring a share data PDU of type {:#04x}", data.kind);
                Ok(None)
            }
        }
    }

    /// The dynamic virtual channel, which the server opens as soon as the share is
    /// live. Everything it says is answered, because a channel whose Create Request
    /// goes unanswered is never opened.
    async fn on_dynamic(&mut self, payload: &[u8]) -> Result<()> {
        let reply = {
            let Self { chunks, incoming, control, caps, .. } = self;
            let Some(pdu) = chunks.push(payload)? else {
                return Ok(());
            };
            let Some(message) = incoming.push(pdu)? else {
                return Ok(());
            };
            answer(message, control, caps)?
        };
        if let Some(reply) = reply {
            self.write_channel(&reply).await?;
        }
        // Display Control is usable once its capabilities have arrived, and a size
        // asked for before then has been waiting for exactly this.
        if !self.resize_ready
            && self.control.is_some()
            && let Some(caps) = self.caps
        {
            self.resize_ready = true;
            self.send(Event::ResizeReady { max_area: caps.area }).await;
            self.send_layout().await?;
        }
        Ok(())
    }

    /// Hand the caller an event, after every rectangle painted before it — waiting
    /// for room in the queue if the caller is behind.
    async fn send(&mut self, event: Event) {
        for rect in std::mem::take(&mut self.damage) {
            if !self.deliver(Event::Paint(rect)).await {
                return;
            }
        }
        self.deliver(event).await;
    }

    /// One event into the queue, waiting for room. `false` means it was not
    /// delivered and neither will anything after it be.
    ///
    /// The wait ends early when the session has been asked to stop, because the
    /// command that asked sits in a queue this thread reads only between events:
    /// waiting here for a caller that is no longer reading would be waiting for a
    /// caller that has already gone. The event is dropped in that case — the loop
    /// picks the shutdown up on its next turn and disconnects — and so is a session
    /// whose receiver is closed, which is a caller that stopped listening while
    /// keeping its `Session`.
    async fn deliver(&mut self, event: Event) -> bool {
        let Self { stop, events, .. } = self;
        tokio::select! {
            biased;
            _ = stop.wait_for(|&stop| stop) => false,
            sent = events.send(event) => sent.is_ok(),
        }
    }

    /// Record a painted rectangle. It goes to the caller at once if the queue has
    /// room, and otherwise folds into what is already waiting: one overlapping an
    /// earlier rectangle becomes their union, and past [`DAMAGE_CAP`] everything
    /// collapses to one bounding box — coarser, never longer.
    fn paint(&mut self, rect: Rect) {
        let union = |a: Rect, b: Rect| {
            let (x, y) = (a.x.min(b.x), a.y.min(b.y));
            let right = (a.x + a.width).max(b.x + b.width);
            let bottom = (a.y + a.height).max(b.y + b.height);
            Rect { x, y, width: right - x, height: bottom - y }
        };
        let overlaps = |a: &Rect, b: &Rect| {
            a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
        };
        if let Some(waiting) = self.damage.iter_mut().find(|waiting| overlaps(waiting, &rect)) {
            *waiting = union(*waiting, rect);
        } else if self.damage.len() >= DAMAGE_CAP {
            let whole = self.damage.drain(..).fold(rect, union);
            self.damage.push(whole);
        } else {
            self.damage.push(rect);
        }
        self.try_send_damage();
    }

    /// Send waiting rectangles while the queue has room, keeping the rest.
    fn try_send_damage(&mut self) {
        while let Some(&rect) = self.damage.first() {
            match self.events.try_send(Event::Paint(rect)) {
                Ok(()) => {
                    self.damage.remove(0);
                }
                Err(TrySendError::Full(_)) => break,
                Err(TrySendError::Closed(_)) => self.damage.clear(),
            }
        }
    }

    /// One frame out, bounded so a host that stops reading cannot hold this thread.
    async fn write(&mut self, frame: &[u8]) -> Result<()> {
        if frame.is_empty() {
            return Ok(());
        }
        match tokio::time::timeout(WRITE_TIMEOUT, self.writer.write_all(frame)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow!("the connection to the host failed: {e}")),
            Err(_) => Err(anyhow!(
                "the connection to the host failed: it accepted nothing for {}s",
                WRITE_TIMEOUT.as_secs()
            )),
        }
    }

    /// One PDU out on the I/O channel, which is where everything but an update and
    /// a keystroke goes.
    async fn write_io(&mut self, pdu: &[u8]) -> Result<()> {
        let frame = mcs::send_data_request(self.user, self.io_channel, pdu)?;
        self.write(&frame).await
    }

    /// One PDU out on the static virtual channel, wearing the chunk header that
    /// channel's payloads wear.
    async fn write_channel(&mut self, pdu: &[u8]) -> Result<()> {
        let Some(dynamic) = self.dynamic else {
            return Ok(()); // no channel was asked for, so nothing opened one
        };
        let chunk = channel::pdu(pdu, self.share.chunk)?;
        let frame = mcs::send_data_request(self.user, dynamic, &chunk)?;
        self.write(&frame).await
    }

    /// The desktop is now `width` × `height`: the framebuffer starts again, blank,
    /// and the caller is told.
    ///
    /// A size this client cannot afford to hold ends the session instead: the
    /// framebuffer is one allocation, sized by the server.
    async fn redefine_desktop(&mut self, width: u32, height: u32) -> Result<()> {
        affordable(width, height)?;
        self.framebuffer.resize(width, height);
        // Rectangles of the desktop that just went away name pixels that no longer
        // exist; the caller starts over from the resize anyway.
        self.damage.clear();
        self.send(Event::Resize { width, height }).await;
        Ok(())
    }

    /// Send the pending monitor layout, if there is one and a channel to carry it.
    async fn send_layout(&mut self) -> Result<()> {
        if !self.resize_ready {
            return Ok(()); // held until the channel is ready
        }
        let Some((width, height, scale)) = self.pending_resize.take() else {
            return Ok(());
        };
        let Some(control) = self.control else {
            return Ok(()); // the server closed the channel; nothing can carry it
        };
        debug!("rdp: sending a {width}x{height} monitor layout at {scale}%");
        let layout = display::monitor_layout(width, height, scale);
        self.write_channel(&dvc::data(control, &layout)?).await
    }

    /// The Deactivation-Reactivation Sequence: the server tore the desktop down and
    /// is building it again — its answer to a monitor layout, and the only way a
    /// desktop ever changes size.
    ///
    /// `true` means the session was asked to stop part-way through. A server owes
    /// this sequence a reply it can take as long as it likes over — and a server
    /// that never sends one leaves the reads below waiting forever — so every one of
    /// them, the capability exchange included, gives way to a shutdown as the main
    /// loop's does, rather than holding a connection nobody is watching until a write
    /// finally times out.
    async fn reactivate(&mut self) -> Result<bool> {
        debug!("rdp: the server deactivated the desktop; reactivating");
        let demand = loop {
            let Self { stop, frames, frame, .. } = self;
            let read = tokio::select! {
                biased;
                _ = stop.wait_for(|&stop| stop) => return Ok(true),
                read = frames.next(frame) => read,
            };
            read?;
            // Updates for a desktop that is about to be replaced, and whatever else
            // the slow path carries: none of it is what this is waiting for. What is
            // dropped with them is asked for again below.
            if fastpath::is_output(self.frame[0]) {
                continue;
            }
            let mcs::Indication::Data(data) = mcs::send_data_indication(&self.frame)? else {
                bail!("the host left the conference while rebuilding the desktop");
            };
            if data.channel != self.io_channel {
                continue;
            }
            if let Pdu::DemandActive(body) = share::decode(data.payload)? {
                break DemandActive::decode(body)?;
            }
        };
        info!("rdp: reactivated, desktop {}x{}", demand.width, demand.height);

        let Self { frames, writer, frame, user, io_channel, stop, .. } = self;
        // The same wait as above, for the same reason: the capability exchange ends
        // with a Font Map the server owes and may never send, and a session nobody is
        // watching must not be held open by it.
        tokio::select! {
            biased;
            _ = stop.wait_for(|&stop| stop) => return Ok(true),
            activated = connect::activate(frames, writer, frame, *user, *io_channel, &demand) => {
                activated?;
            }
        }

        self.share = Share::from(&demand);
        self.fragments = Fragments::new(demand.multifragment);
        self.redefine_desktop(u32::from(demand.width), u32::from(demand.height)).await?;
        // The desktop is blank and the updates that would have filled it were read
        // past above, so the repaint is asked for here rather than waited for.
        self.refresh().await?;
        Ok(false)
    }

    /// `first`, then whatever else is already queued behind it, with consecutive
    /// input batched into as few PDUs as it fits. `true` means stop.
    async fn on_commands(
        &mut self,
        first: Command,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Result<bool> {
        let mut batch = Vec::new();
        let mut next = Some(first);
        let mut taken = 0;
        while let Some(command) = next.take() {
            taken += 1;
            match command {
                Command::Input(event) => batch.push(event),
                other => {
                    // Order is kept: input queued before a refresh goes out before it.
                    self.send_input(&mut batch).await?;
                    match other {
                        Command::Shutdown => return Ok(true),
                        Command::Refresh => self.refresh().await?,
                        Command::Resize { width, height, scale_percent } => {
                            self.pending_resize = Some((width, height, scale_percent));
                            self.send_layout().await?;
                        }
                        Command::Input(_) => unreachable!("matched above"),
                    }
                }
            }
            if taken < COMMANDS_PER_TURN {
                next = commands.try_recv().ok();
            }
        }
        self.send_input(&mut batch).await?;
        Ok(false)
    }

    async fn send_input(&mut self, batch: &mut Vec<input::Event>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        for pdu in input::pdus(batch).collect::<Vec<_>>() {
            self.write(&pdu).await?;
        }
        batch.clear();
        Ok(())
    }

    /// Ask the server to paint the whole desktop again, by whichever means it said
    /// it would answer. A server that offers neither is asked for nothing.
    async fn refresh(&mut self) -> Result<()> {
        let (width, height) = (self.share.width, self.share.height);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let pdus = desktop::repaint(
            self.user,
            self.share.id,
            narrow(width),
            narrow(height),
            self.share.refresh_rect,
            self.share.suppress_output,
        );
        for pdu in pdus {
            self.write_io(&pdu).await?;
        }
        Ok(())
    }

    /// Leave the way a client that meant to leaves: an MCS Disconnect Provider
    /// Ultimatum, which the server reads as the user disconnecting — the session
    /// stays on the host, logged on, for the next connection — then a TLS close.
    /// Best effort and bounded, because the connection may already be gone.
    async fn disconnect(&mut self) {
        let goodbye = async {
            let ultimatum = mcs::disconnect_provider_ultimatum(mcs::Reason::USER_REQUESTED);
            let _ = self.writer.write_all(&ultimatum).await;
            let _ = self.writer.shutdown().await;
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), goodbye).await;
    }
}

/// One reassembled fast-path update, decoded.
///
/// Rectangles of the desktop are painted straight into the framebuffer and their
/// bounds appended to `painted`; a cursor comes back to be handed on. Everything
/// else a server sends on this path — drawing orders this client did not ask for, a
/// palette a 32-bit session has no use for, where the server thinks the pointer is —
/// is nothing this session acts on.
fn draw(
    update: Update<'_>,
    scratch: &mut bitmap::Scratch,
    pixels: &mut Vec<u8>,
    cursors: &mut pointer::Cache,
    framebuffer: &Framebuffer,
    share: &Share,
    painted: &mut Vec<Rect>,
) -> Result<Option<Cursor>> {
    match update.code {
        fastpath::BITMAP => {
            for rectangle in bitmap::update(update.data)? {
                rectangle.decode(scratch, pixels)?;
                let rect = Rect {
                    x: u32::from(rectangle.x),
                    y: u32::from(rectangle.y),
                    width: u32::from(rectangle.paint_width),
                    height: u32::from(rectangle.paint_height),
                };
                if !rect.is_empty() && framebuffer.blit(pixels, rect) {
                    painted.push(rect);
                }
            }
            Ok(None)
        }
        fastpath::POINTER_HIDDEN
        | fastpath::POINTER_DEFAULT
        | fastpath::POINTER_POSITION
        | fastpath::COLOR_POINTER
        | fastpath::CACHED_POINTER
        | fastpath::NEW_POINTER
        | fastpath::LARGE_POINTER => Ok(match cursors.update(update.code, update.data)? {
            Pointer::Hidden => Some(Cursor::Hidden),
            Pointer::Default => Some(Cursor::Default),
            Pointer::Shape(shape) => Some(Cursor::Image(shape.into())),
            // A client that draws its own pointer has no use for where the server
            // has put one.
            Pointer::Position { .. } => None,
        }),
        other => {
            let desktop = (share.width, share.height);
            debug!("rdp: ignoring a fast-path update of type {other:#x} on a {desktop:?} desktop");
            Ok(None)
        }
    }
}

/// What to say back to one dynamic channel PDU.
///
/// The one channel this client takes is Display Control; every other name a Windows
/// host offers — a printer, a smart card, a camera — is refused by name, which is
/// what a client with nothing behind them does.
fn answer(
    message: dvc::Message<'_>,
    control: &mut Option<u32>,
    caps: &mut Option<display::Capabilities>,
) -> Result<Option<Vec<u8>>> {
    Ok(match message {
        dvc::Message::Capabilities { version } => Some(dvc::capabilities_response(version)),
        dvc::Message::Create { channel, name } => {
            let wanted = name == display::CHANNEL_NAME;
            if wanted {
                debug!("rdp: the host opened Display Control on dynamic channel {channel}");
                *control = Some(channel);
            }
            let status = if wanted { dvc::ACCEPTED } else { dvc::NO_LISTENER };
            Some(dvc::create_response(channel, status))
        }
        dvc::Message::Close { channel } => {
            if *control == Some(channel) {
                *control = None;
                *caps = None;
            }
            None
        }
        dvc::Message::Data { channel, data } => {
            if *control == Some(channel) {
                let read = display::capabilities(data)?;
                debug!("rdp: Display Control will lay out {read:?}");
                *caps = Some(read);
            }
            None
        }
    })
}

/// A desktop dimension as the `u16` the protocol counts in, saturating rather than
/// wrapping: nothing real exceeds RDP's own 8192 a side.
fn narrow(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

/// A desktop size the server named, refused before anything is allocated for it —
/// see [`MAX_DESKTOP_BYTES`]. Both a real desktop's size and an absurd one are
/// legal on the wire, so the difference is made here.
fn affordable(width: u32, height: u32) -> Result<()> {
    let bytes = usize::try_from(width)
        .ok()
        .zip(usize::try_from(height).ok())
        .and_then(|(width, height)| width.checked_mul(height))
        .and_then(|pixels| pixels.checked_mul(4));
    match bytes {
        Some(bytes) if bytes <= MAX_DESKTOP_BYTES => Ok(()),
        _ => Err(anyhow!(
            "the server asked for a {width}x{height} desktop, which is more than the {} MiB \
             this client will hold",
            MAX_DESKTOP_BYTES >> 20
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server names the desktop, and this client allocates a framebuffer from what
    /// it says. Both numbers are legal on the wire well past any real screen.
    #[test]
    fn a_desktop_too_large_to_hold_is_refused_rather_than_allocated() {
        affordable(1920, 1080).expect("an ordinary desktop");
        affordable(15360, 4320).expect("two 8K monitors side by side is still real");
        affordable(0, 0).expect("a desktop with no pixels costs nothing");

        // The largest desktop a negotiation can name, and past what this client
        // will hold.
        let err = affordable(32766, 32766).expect_err("4 GiB");
        assert!(format!("{err}").contains("32766x32766"), "{err}");
        affordable(65535, 65535).expect_err("17 GB");
        affordable(u32::MAX, u32::MAX).expect_err("the arithmetic itself must not wrap");
    }

    /// The one channel this client takes, out of the dozen a Windows host offers.
    #[test]
    fn only_display_control_is_taken_and_every_other_channel_is_refused_by_name() {
        let (mut control, mut caps) = (None, None);
        let create = |name| dvc::Message::Create { channel: 11, name };
        let reply = answer(create("AUDIO_PLAYBACK_DVC"), &mut control, &mut caps).unwrap();
        assert_eq!(reply, Some(dvc::create_response(11, dvc::NO_LISTENER)));
        assert_eq!(control, None, "a channel nothing listens on is not remembered");

        let reply = answer(create(display::CHANNEL_NAME), &mut control, &mut caps).unwrap();
        assert_eq!(reply, Some(dvc::create_response(11, dvc::ACCEPTED)));
        assert_eq!(control, Some(11));
    }

    /// A channel the server closes takes its capabilities with it: what it said it
    /// would lay out was about a channel that no longer exists.
    #[test]
    fn closing_display_control_forgets_what_it_said_it_would_do() {
        let said = display::Capabilities { monitors: 1, area: 4 };
        let (mut control, mut caps) = (Some(11), Some(said));
        let elsewhere = dvc::Message::Close { channel: 12 };
        assert_eq!(answer(elsewhere, &mut control, &mut caps).unwrap(), None);
        assert_eq!(control, Some(11), "another channel closing says nothing about this one");
        answer(dvc::Message::Close { channel: 11 }, &mut control, &mut caps).unwrap();
        assert_eq!((control, caps), (None, None));
    }
}
