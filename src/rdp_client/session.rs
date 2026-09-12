//! The session: its configuration, its thread, and the loop that drives it.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use log::{debug, info, warn};
use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use tokio::time::Duration;

use super::connect::{self, Connected, Joined};
use super::error::Error;
use super::framebuffer::{Framebuffer, Rect, affordable};
use super::gfx::{self, Graphics};
use super::input::{Clipboard, Command, Input};
use super::pointer::Cursor;
use super::proto::capabilities::DemandActive;
use super::proto::channel::Chunk;
use super::proto::fastpath::{self, Fragments, Update};
use super::proto::frame::Frames;
use super::proto::gcc::Channel;
use super::proto::pointer::{self, Pointer};
use super::proto::share::{self, Pdu};
use super::proto::{bitmap, channel, cliprdr, desktop, display, dvc, input, mcs, tls};
use super::proto::gfx as gfx_proto;

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
    /// A server answers a monitor layout by resizing the desktop: with a graphics
    /// reset under [`Connect::egfx`], and without it with a Deactivation-Reactivation
    /// Sequence that tears the desktop and the capability set down and builds them
    /// again. Either way this client sees one [`Event::Resize`] at the end of it.
    pub resize: bool,
    /// Whether to offer the graphics pipeline (MS-RDPEGFX).
    ///
    /// Offered, a Windows host draws the desktop through surfaces on a dynamic
    /// channel of its own, marks every frame's end — [`Event::Frame`] — and answers a
    /// monitor layout with a graphics reset rather than a reactivation. Not offered,
    /// the host draws with bitmap updates on the share, which is the path every
    /// other server takes anyway.
    pub egfx: bool,
    /// Whether to open MS-RDPECLIP, which is what makes the clipboard side of
    /// [`Input`] do anything.
    ///
    /// A server opens the channel a moment after the desktop and says so with
    /// [`Event::ClipboardReady`]; nothing crosses it until one end announces a copy.
    /// A session that did not ask for the channel reports none of the clipboard
    /// events and drops every clipboard command.
    pub clipboard: bool,
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
    /// Bitmap updates carry no frame boundary, so on a session without
    /// [`Event::Frame`] nothing says where one picture ends and the next begins; a
    /// consumer that needs to present coherent frames paces them itself.
    Paint(Rect),
    /// The server finished a frame: every [`Event::Paint`] since the last `Frame`
    /// belongs to one coherent picture. Sent only when the server says so itself —
    /// the graphics pipeline's EndFrame — never guessed from timing. The bitmap path
    /// marks no frames, so a consumer keeps whatever pacing it had and treats this as
    /// the upgrade it is.
    Frame,
    /// The server confirmed the graphics pipeline, so every [`Event::Paint`] from
    /// here on arrives inside a frame that ends in an [`Event::Frame`]. Sent before
    /// the first such paint, so a consumer pacing frames itself stops guessing
    /// before there is a frame to cut in half.
    FramesMarked,
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
    /// The remote's clipboard channel is open, so the clipboard side of [`Input`]
    /// now has somewhere to go. Only ever sent on a session configured with
    /// [`Connect::clipboard`], and not at all by a server that does not implement
    /// MS-RDPECLIP.
    ///
    /// The caller answers by advertising what its own clipboard holds —
    /// [`Input::advertise_clipboard`] — including nothing, which is what tells the
    /// remote there is a clipboard on this end at all.
    ClipboardReady,
    /// The remote copied something, and these are the format ids it can produce it
    /// in. Nothing has been transferred: the bytes cost an
    /// [`Input::request_clipboard`] and a second round trip.
    ClipboardFormats(Vec<u32>),
    /// The bytes of the format [`Input::request_clipboard`] asked for.
    ClipboardData(Vec<u8>),
    /// The remote would not produce the bytes that were asked for, and does not say
    /// why. A Windows peer answers a second ask for the same format often enough
    /// that a bounded retry is worth having — see [`Input::request_clipboard`].
    ClipboardRefused,
    /// A remote copy too large for this client to hold, dropped unread — see
    /// [`channel::MAX_PDU`]. `bytes` is the size it announced, which is all there is
    /// left to report, and the honest thing to report: a truncated paste cannot be
    /// told from a whole one.
    ClipboardOversized { bytes: u64 },
    /// The remote is pasting and **is waiting** for the bytes of `format`. Every one
    /// of these has to be answered with [`Input::send_clipboard`], including with
    /// nothing — the application on the far end is blocked inside its own paste
    /// handler until it is.
    ClipboardWanted { format: u32 },
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
    Active::new(connected, &config, framebuffer, events, stop).run(commands).await
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
/// identifier, the size and the limits are all the new share's. The size alone
/// also moves with a graphics pipeline reset, which resizes the output without
/// rebuilding the share.
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
    /// that asked to be resizable or for the graphics pipeline.
    dynamic: Option<Joined>,
    /// The static virtual channel the clipboard travels on, for a session that asked
    /// for one.
    clipboard: Option<Joined>,
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
    /// The clipboard channel's own chunks. One reassembler per channel: the pieces
    /// of two PDUs never interleave on one channel, and these are a separate
    /// sequence from the dynamic channel's.
    clip_chunks: channel::Reassembly,
    /// The dynamic channels this client takes, by the numbers the server gave them.
    dynamics: Dynamics,
    /// The graphics pipeline's surfaces and decompressor, for a session that offered
    /// it. Outlives a reactivation with the channel it belongs to.
    graphics: Option<Graphics>,
    /// Whether [`Event::ResizeReady`] has gone out.
    resize_ready: bool,
    /// The most recent size asked for before the channel was ready — only the most
    /// recent, since a resize supersedes every earlier one rather than queueing
    /// behind it.
    pending_resize: Option<(u32, u32, u32)>,
    /// Whether the server has sent Monitor Ready, which is what opens the clipboard:
    /// nothing may be said on that channel before this end's capabilities answer it.
    clip_ready: bool,
    /// The most recent formats offered before that happened — the most recent only,
    /// for [`Self::pending_resize`]'s reason: each advertisement replaces the last.
    pending_formats: Option<Vec<u32>>,
    /// PDUs that arrived on a static virtual channel while the share was not live —
    /// the capability exchange, where a server opens the clipboard, and the wait for
    /// a Demand Active before it. Acted on in order once it is, because nothing on a
    /// channel can be asked for again — see [`connect::activate`].
    deferred: Vec<(u16, Vec<u8>)>,

    framebuffer: &'a Framebuffer,
    events: &'a mpsc::Sender<Event>,
    /// Painted rectangles not yet handed to the caller, folded together while the
    /// event queue is full — see [`EVENT_QUEUE`].
    damage: Vec<Rect>,
    /// Raised once the session has been asked to stop, which is what lets a wait
    /// for room in the caller's queue end — see [`Self::deliver`].
    stop: watch::Receiver<bool>,
}

/// The dynamic channels this client takes, and the numbers the server's Create
/// Requests gave them.
#[derive(Debug, Default, PartialEq, Eq)]
struct Dynamics {
    /// Whether Display Control is wanted at all — [`Connect::resize`].
    resize: bool,
    /// Whether the Graphics channel is wanted at all — [`Connect::egfx`].
    egfx: bool,
    /// Display Control, once the server has opened it.
    control: Option<u32>,
    /// What Display Control said it would lay out, which arrives after the channel
    /// is open and before a layout may be sent.
    caps: Option<display::Capabilities>,
    /// The Graphics channel, once the server has opened it.
    graphics: Option<u32>,
}

impl<'a> Active<'a> {
    fn new(
        connected: Connected,
        config: &Connect,
        framebuffer: &'a Framebuffer,
        events: &'a mpsc::Sender<Event>,
        stop: watch::Receiver<bool>,
    ) -> Self {
        let dynamic = connected.channel(Channel::DYNAMIC);
        let clipboard = connected.channel(Channel::CLIPBOARD);
        let Connected { frames, writer, user, io_channel, demand, deferred, .. } = connected;
        Self {
            frames,
            writer,
            frame: Vec::new(),
            user,
            io_channel,
            dynamic,
            clipboard,
            share: Share::from(&demand),
            fragments: Fragments::new(demand.multifragment),
            scratch: bitmap::Scratch::default(),
            pixels: Vec::new(),
            cursors: pointer::Cache::new(),
            chunks: channel::Reassembly::new(),
            incoming: dvc::Incoming::new(),
            clip_chunks: channel::Reassembly::new(),
            dynamics: Dynamics { resize: config.resize, egfx: config.egfx, ..Dynamics::default() },
            graphics: config.egfx.then(Graphics::new),
            resize_ready: false,
            pending_resize: None,
            clip_ready: false,
            pending_formats: None,
            deferred,
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
        // What arrived on a channel in the same window was kept instead, because a
        // channel's PDUs cannot be asked for again.
        for (channel, payload) in std::mem::take(&mut self.deferred) {
            self.on_channel(channel, &payload).await?;
        }
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
            mcs::Indication::Data(data) if data.channel == self.io_channel => {
                self.on_share(data.payload).await
            }
            mcs::Indication::Data(data) => {
                self.on_channel(data.channel, data.payload).await?;
                Ok(None)
            }
        }
    }

    /// One PDU on a static virtual channel, by the number the server gave it.
    ///
    /// The same routing wherever a channel's PDU is read from — the main loop, the
    /// capability exchange it was kept from, the reactivation that would otherwise
    /// have dropped it — so that a channel stays open across everything the share
    /// does.
    async fn on_channel(&mut self, channel: u16, payload: &[u8]) -> Result<()> {
        if self.dynamic.is_some_and(|dynamic| dynamic.number == channel) {
            return self.on_dynamic(payload).await;
        }
        if self.clipboard.is_some_and(|clipboard| clipboard.number == channel) {
            return self.on_clipboard(payload).await;
        }
        // A channel this client neither asked for nor joined. A server does not send
        // one, and a PDU on one is nothing this session can act on.
        debug!("rdp: ignoring {} bytes on channel {channel}", payload.len());
        Ok(())
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
        let (replies, updates) = {
            let Self { chunks, incoming, dynamics, graphics, framebuffer, .. } = self;
            let pdu = match chunks.push(payload)? {
                Chunk::Whole(pdu) => pdu,
                Chunk::Partial => return Ok(()),
                // Every PDU on this channel is one chunk of a dynamic channel PDU, so
                // this is a server that has lost the thread rather than a payload
                // worth having.
                Chunk::Dropped { length } => {
                    warn!("rdp: dropping a {length}-byte dynamic channel PDU, which is absurd");
                    return Ok(());
                }
            };
            let Some(message) = incoming.push(pdu)? else {
                return Ok(());
            };
            match message {
                // The desktop itself, which is the framebuffer's business and not a
                // reply's.
                dvc::Message::Data { channel, data } if dynamics.graphics == Some(channel) => {
                    let Some(graphics) = graphics else {
                        bail!("the host drew on a graphics channel this client never accepted");
                    };
                    (Vec::new(), graphics.receive(data, framebuffer)?)
                }
                // The channel is gone, and with it every surface and the history the
                // compressor was working from; a channel opened again starts afresh.
                dvc::Message::Close { channel } if dynamics.graphics == Some(channel) => {
                    debug!("rdp: the host closed the graphics channel");
                    dynamics.graphics = None;
                    *graphics = Some(Graphics::new());
                    (Vec::new(), Vec::new())
                }
                message => (answer(message, dynamics)?, Vec::new()),
            }
        };
        if let Some(dynamic) = self.dynamic {
            for reply in replies {
                self.write_channel(dynamic, &reply).await?;
            }
        }
        for update in updates {
            match update {
                // The host confirmed the pipeline, so every paint from here on
                // arrives inside a marked frame; said before the first of them.
                gfx::Update::Confirmed => self.send(Event::FramesMarked).await,
                // The framebuffer is already the new size; the caller is told now,
                // before the paints that follow in the same PDU. The share's size
                // follows too: a Refresh Rect asked for later — and one is, after
                // every resize — is in the coordinates of this desktop, not the one
                // the Demand Active described.
                gfx::Update::Reset { width, height } => {
                    info!("rdp: graphics reset, desktop {width}x{height}");
                    self.share.width = width;
                    self.share.height = height;
                    self.announce_desktop(width, height).await;
                }
                gfx::Update::Paint(rect) => self.paint(rect),
                gfx::Update::Frame { id, decoded } => {
                    self.send(Event::Frame).await;
                    self.acknowledge_frame(id, decoded).await?;
                }
            }
        }
        // Display Control is usable once its capabilities have arrived, and a size
        // asked for before then has been waiting for exactly this.
        if !self.resize_ready
            && self.dynamics.control.is_some()
            && let Some(caps) = self.dynamics.caps
        {
            self.resize_ready = true;
            self.send(Event::ResizeReady { max_area: caps.area }).await;
            self.send_layout().await?;
        }
        Ok(())
    }

    /// The acknowledgement every EndFrame is owed: without it a Windows host
    /// throttles, then stops sending frames altogether.
    async fn acknowledge_frame(&mut self, frame: u32, decoded: u32) -> Result<()> {
        let (Some(channel), Some(dynamic)) = (self.dynamics.graphics, self.dynamic) else {
            return Ok(()); // the channel closed under the frame; nothing to answer on
        };
        let ack = gfx_proto::frame_acknowledge(frame, decoded);
        self.write_channel(dynamic, &dvc::data(channel, &ack)?).await
    }

    /// The clipboard channel. Both ends announce a copy and neither transfers
    /// anything until somebody pastes, so most of what arrives here is answered by
    /// handing the caller one event and waiting.
    ///
    /// What crosses this boundary is a format id and bytes. Which format is text, and
    /// what its bytes mean, belong to the caller — see [`crate::rdp_clipboard`].
    async fn on_clipboard(&mut self, payload: &[u8]) -> Result<()> {
        // Read inside this borrow and acted on outside it: the answers below are
        // written to the very channel the chunk came off.
        let (reply, event) = {
            let pdu = match self.clip_chunks.push(payload)? {
                Chunk::Whole(pdu) => pdu,
                Chunk::Partial => return Ok(()),
                // A copy on the far end larger than this client holds. Almost
                // certainly the answer to a request, which is what the caller is
                // waiting on, so it is reported as a size rather than a silence —
                // and the channel carries on.
                Chunk::Dropped { length } => {
                    warn!("rdp: a {length}-byte clipboard PDU is more than this client holds");
                    self.send(Event::ClipboardOversized { bytes: length as u64 }).await;
                    return Ok(());
                }
            };
            answer_clipboard(pdu)?
        };
        if let (Some(reply), Some(clipboard)) = (reply, self.clipboard) {
            self.write_channel(clipboard, &reply).await?;
        }
        // The reply above is this end's capabilities, and it has gone: from here the
        // channel carries what the caller asks it to, including anything it asked
        // for early — see [`Self::send_clipboard`].
        if matches!(event, Some(Event::ClipboardReady)) {
            self.clip_ready = true;
            if let Some(formats) = self.pending_formats.take() {
                self.send_clipboard(Clipboard::Advertise(formats)).await?;
            }
        }
        if let Some(event) = event {
            self.send(event).await;
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

    /// One PDU out on a static virtual channel, split into as many chunks as that
    /// channel takes and each wearing the header those chunks wear.
    async fn write_channel(&mut self, channel: Joined, pdu: &[u8]) -> Result<()> {
        for chunk in channel::chunks(pdu, self.share.chunk, channel.flags)? {
            let frame = mcs::send_data_request(self.user, channel.number, &chunk)?;
            self.write(&frame).await?;
        }
        Ok(())
    }

    /// The desktop is now `width` × `height`: the framebuffer starts again, blank,
    /// and the caller is told.
    ///
    /// A size this client cannot afford to hold ends the session instead: the
    /// framebuffer is one allocation, sized by the server.
    async fn redefine_desktop(&mut self, width: u32, height: u32) -> Result<()> {
        affordable(width, height)?;
        self.framebuffer.resize(width, height);
        self.announce_desktop(width, height).await;
        Ok(())
    }

    /// The framebuffer has been resized and cleared; tell the caller.
    async fn announce_desktop(&mut self, width: u32, height: u32) {
        // Rectangles of the desktop that just went away name pixels that no longer
        // exist; the caller starts over from the resize anyway.
        self.damage.clear();
        self.send(Event::Resize { width, height }).await;
    }

    /// Send the pending monitor layout, if there is one and a channel to carry it.
    async fn send_layout(&mut self) -> Result<()> {
        if !self.resize_ready {
            return Ok(()); // held until the channel is ready
        }
        let Some((width, height, scale)) = self.pending_resize.take() else {
            return Ok(());
        };
        let Some(control) = self.dynamics.control else {
            return Ok(()); // the server closed the channel; nothing can carry it
        };
        let Some(dynamic) = self.dynamic else {
            return Ok(()); // no channel was asked for, so nothing opened one
        };
        debug!("rdp: sending a {width}x{height} monitor layout at {scale}%");
        let layout = display::monitor_layout(width, height, scale);
        self.write_channel(dynamic, &dvc::data(control, &layout)?).await
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
            // Updates for a desktop that is about to be replaced: not what this is
            // waiting for, and what is dropped with them is asked for again below.
            if fastpath::is_output(self.frame[0]) {
                continue;
            }
            // Read out of `self` and acted on outside it, as the main loop's frames
            // are: answering one writes to the very channel it came off.
            let frame = std::mem::take(&mut self.frame);
            let demand = self.reactivating(&frame).await;
            self.frame = frame;
            if let Some(demand) = demand? {
                break demand;
            }
        };
        info!("rdp: reactivated, desktop {}x{}", demand.width, demand.height);

        let Self { frames, writer, frame, user, io_channel, stop, .. } = self;
        // The same wait as above, for the same reason: the capability exchange ends
        // with a Font Map the server owes and may never send, and a session nobody is
        // watching must not be held open by it.
        let deferred = tokio::select! {
            biased;
            _ = stop.wait_for(|&stop| stop) => return Ok(true),
            activated = connect::activate(frames, writer, frame, *user, *io_channel, &demand) => {
                activated?
            }
        };
        self.deferred.extend(deferred);

        self.share = Share::from(&demand);
        self.fragments = Fragments::new(demand.multifragment);
        self.redefine_desktop(u32::from(demand.width), u32::from(demand.height)).await?;
        // The desktop is blank and the updates that would have filled it were read
        // past above, so the repaint is asked for here rather than waited for.
        self.refresh().await?;
        // And what a channel carried while all that happened, in the order it came.
        for (channel, payload) in std::mem::take(&mut self.deferred) {
            self.on_channel(channel, &payload).await?;
        }
        Ok(false)
    }

    /// One frame read while the server is rebuilding the desktop. `Some` is the
    /// Demand Active that ends the wait.
    ///
    /// A resize is not the whole session, and a channel is not the share's: what
    /// arrives on one while this is waiting is either answered here or kept for
    /// afterwards, never dropped. The clipboard is answered here, because a Format
    /// Data Request is a remote application stopped inside its own paste and it has
    /// no idea a desktop is being rebuilt. Every other channel waits for the share
    /// to be live, where acting on one cannot ask a server busy rebuilding a desktop
    /// for another layout of it.
    ///
    /// Only the share's own PDUs are read past — the updates for a desktop that is
    /// going away, and the acks for one.
    async fn reactivating(&mut self, frame: &[u8]) -> Result<Option<DemandActive>> {
        let mcs::Indication::Data(data) = mcs::send_data_indication(frame)? else {
            bail!("the host left the conference while rebuilding the desktop");
        };
        if self.clipboard.is_some_and(|clipboard| clipboard.number == data.channel) {
            self.on_clipboard(data.payload).await?;
            return Ok(None);
        }
        if data.channel != self.io_channel {
            self.deferred.push((data.channel, data.payload.to_vec()));
            return Ok(None);
        }
        match share::decode(data.payload)? {
            Pdu::DemandActive(body) => Ok(Some(DemandActive::decode(body)?)),
            _ => Ok(None),
        }
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
                        Command::Clipboard(what) => self.send_clipboard(what).await?,
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

    /// One thing the caller wants said on the clipboard channel.
    ///
    /// Dropped when there is no channel — a session that did not ask for one, or a
    /// server that never opened it — which is the same bargain every other input
    /// makes on a session that cannot carry it.
    ///
    /// Held when the channel is joined but the server has not sent Monitor Ready
    /// yet. The caller learns the clipboard is usable from [`Event::ClipboardReady`],
    /// but it can be told the *session* is up before that and copy something, and a
    /// format list ahead of the capability exchange is a list sent before either end
    /// has said which shape its lists are in. Only an advertisement can be early —
    /// the other two answer a server PDU, which cannot arrive before it opens the
    /// channel — so holding the most recent one is holding all there is.
    async fn send_clipboard(&mut self, what: Clipboard) -> Result<()> {
        let Some(clipboard) = self.clipboard else {
            return Ok(());
        };
        if !self.clip_ready {
            if let Clipboard::Advertise(formats) = what {
                debug!("rdp: holding clipboard formats {formats:?} until the channel opens");
                self.pending_formats = Some(formats);
            }
            return Ok(());
        }
        let pdu = match what {
            Clipboard::Advertise(formats) => {
                debug!("rdp: advertising clipboard formats {formats:?}");
                cliprdr::format_list(&formats)
            }
            Clipboard::Request(format) => cliprdr::data_request(format),
            Clipboard::Respond(data) => cliprdr::data_response(data.as_deref()),
        };
        self.write_channel(clipboard, &pdu).await
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

/// What to say back to one clipboard PDU, and what to tell the caller about it.
///
/// A pure function for the same reason [`answer`] is: everything this channel does is
/// decided here, and a decision on its own is a thing a test can make.
fn answer_clipboard(pdu: &[u8]) -> Result<(Option<Vec<u8>>, Option<Event>)> {
    Ok(match cliprdr::decode(pdu)? {
        // Read and said out loud, and nothing more: what a server can do does not
        // change what this client speaks.
        cliprdr::Message::Capabilities { version, flags } => {
            debug!("rdp: the host's clipboard is version {version}, flags {flags:#x}");
            (None, None)
        }
        // This end's capabilities go first and they go from here: they are what
        // settles the shape of every format list after them, and the caller has
        // nothing to say about them.
        cliprdr::Message::MonitorReady => {
            debug!("rdp: the host opened the clipboard channel");
            (Some(cliprdr::capabilities()), Some(Event::ClipboardReady))
        }
        cliprdr::Message::Formats(list) => {
            let formats = cliprdr::formats(list)?;
            debug!("rdp: the remote clipboard now holds {formats:?}");
            (Some(cliprdr::format_list_response()), Some(Event::ClipboardFormats(formats)))
        }
        cliprdr::Message::DataRequest { format } => (None, Some(Event::ClipboardWanted { format })),
        cliprdr::Message::Data(Some(bytes)) => (None, Some(Event::ClipboardData(bytes.to_vec()))),
        cliprdr::Message::Data(None) => (None, Some(Event::ClipboardRefused)),
        // A list this end sent that the server would not take. Nothing to retry — the
        // next copy sends another one — and worth a line, because it is the far end
        // saying it has ignored a copy.
        cliprdr::Message::ListResponse { ok } => {
            if !ok {
                warn!("rdp: the host refused this end's clipboard advertisement");
            }
            (None, None)
        }
        cliprdr::Message::Ignored(kind) => {
            debug!("rdp: ignoring a clipboard PDU of type {kind:#06x}");
            (None, None)
        }
    })
}

/// What to say back to one dynamic channel PDU — none, one, or two replies.
///
/// Two channels are taken, each only when the session asked for what rides on it:
/// Display Control for [`Connect::resize`], and the Graphics channel for
/// [`Connect::egfx`], whose acceptance is followed at once by this client's
/// capabilities, because a server waits for those before it draws anything. Every
/// other name a Windows host offers — a printer, a smart card, a camera — is refused
/// by name, which is what a client with nothing behind them does.
///
/// The Graphics channel's own data does not come here: it is the desktop, and the
/// session hands it to the compositor instead.
fn answer(message: dvc::Message<'_>, dynamics: &mut Dynamics) -> Result<Vec<Vec<u8>>> {
    Ok(match message {
        dvc::Message::Capabilities { version } => vec![dvc::capabilities_response(version)],
        dvc::Message::Create { channel, name } => {
            if name == display::CHANNEL_NAME && dynamics.resize {
                debug!("rdp: the host opened Display Control on dynamic channel {channel}");
                dynamics.control = Some(channel);
                vec![dvc::create_response(channel, dvc::ACCEPTED)]
            } else if name == gfx_proto::CHANNEL_NAME && dynamics.egfx {
                debug!("rdp: the host opened the graphics pipeline on dynamic channel {channel}");
                dynamics.graphics = Some(channel);
                // Client-to-server graphics PDUs go raw: the host reads the RDPGFX
                // header off the channel directly, and only the server-to-client
                // direction is bulk-compressed.
                let caps = gfx_proto::caps_advertise();
                vec![dvc::create_response(channel, dvc::ACCEPTED), dvc::data(channel, &caps)?]
            } else {
                vec![dvc::create_response(channel, dvc::NO_LISTENER)]
            }
        }
        dvc::Message::Close { channel } => {
            if dynamics.control == Some(channel) {
                dynamics.control = None;
                dynamics.caps = None;
            }
            Vec::new()
        }
        dvc::Message::Data { channel, data } => {
            if dynamics.control == Some(channel) {
                let read = display::capabilities(data)?;
                debug!("rdp: Display Control will lay out {read:?}");
                dynamics.caps = Some(read);
            }
            Vec::new()
        }
    })
}

/// A desktop dimension as the `u16` the protocol counts in, saturating rather than
/// wrapping: nothing real exceeds RDP's own 8192 a side.
fn narrow(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two channels this client takes, out of the dozen a Windows host offers —
    /// and each only when the session asked for what rides on it.
    #[test]
    fn only_the_channels_asked_for_are_taken_and_every_other_is_refused_by_name() {
        let mut dynamics = Dynamics { resize: true, egfx: true, ..Dynamics::default() };
        let create = |name| dvc::Message::Create { channel: 11, name };
        let reply = answer(create("AUDIO_PLAYBACK_DVC"), &mut dynamics).unwrap();
        assert_eq!(reply, vec![dvc::create_response(11, dvc::NO_LISTENER)]);
        assert_eq!(dynamics.control, None, "a channel nothing listens on is not remembered");

        let reply = answer(create(display::CHANNEL_NAME), &mut dynamics).unwrap();
        assert_eq!(reply, vec![dvc::create_response(11, dvc::ACCEPTED)]);
        assert_eq!(dynamics.control, Some(11));

        // The graphics channel is accepted and, in the same breath, told what this
        // client can take: a server draws nothing until it has heard that.
        let reply = answer(dvc::Message::Create { channel: 12, name: gfx_proto::CHANNEL_NAME }, &mut dynamics).unwrap();
        assert_eq!(reply.len(), 2);
        assert_eq!(reply[0], dvc::create_response(12, dvc::ACCEPTED));
        assert_eq!(reply[1], dvc::data(12, &gfx_proto::caps_advertise()).unwrap());
        assert_eq!(dynamics.graphics, Some(12));

        // A session that asked for neither refuses both by name.
        let mut none = Dynamics::default();
        let reply = answer(create(display::CHANNEL_NAME), &mut none).unwrap();
        assert_eq!(reply, vec![dvc::create_response(11, dvc::NO_LISTENER)]);
        let reply = answer(create(gfx_proto::CHANNEL_NAME), &mut none).unwrap();
        assert_eq!(reply, vec![dvc::create_response(11, dvc::NO_LISTENER)]);
        assert_eq!(none, Dynamics::default());
    }

    /// The opening PDU of the clipboard negotiation is answered from inside the
    /// client — the capabilities are what settle the form of every format list after
    /// them — and the caller is told the channel is live in the same breath.
    #[test]
    fn the_clipboards_opening_pdu_is_answered_here_and_reported_up() {
        let (reply, event) = answer_clipboard(&monitor_ready()).unwrap();
        assert_eq!(reply, Some(cliprdr::capabilities()));
        assert!(matches!(event, Some(Event::ClipboardReady)));
    }

    /// A remote copy: answered on the wire, because a format list is owed a
    /// response, and handed up, because only the caller knows what to ask for.
    #[test]
    fn a_remote_copy_is_acknowledged_and_its_formats_handed_up() {
        let list = cliprdr::format_list(&[13, 1]);
        let (reply, event) = answer_clipboard(&list).unwrap();
        assert_eq!(reply, Some(cliprdr::format_list_response()));
        let Some(Event::ClipboardFormats(formats)) = event else { panic!("{event:?}") };
        assert_eq!(formats, vec![13, 1]);
    }

    /// A paste on the far end. Nothing is answered from here: the bytes are the
    /// caller's, and the caller has to be the one to send them — or to refuse.
    #[test]
    fn a_paste_on_the_remote_is_the_callers_to_answer() {
        let (reply, event) = answer_clipboard(&cliprdr::data_request(13)).unwrap();
        assert_eq!(reply, None);
        assert!(matches!(event, Some(Event::ClipboardWanted { format: 13 })));
    }

    /// The two answers to a read this end asked for, which the caller tells apart
    /// because it treats them differently: bytes are a clipboard, and a refusal is
    /// worth asking again about.
    #[test]
    fn the_two_answers_to_a_read_reach_the_caller_as_different_events() {
        let bytes = vec![b'h', 0, b'i', 0, 0, 0];
        let (reply, event) = answer_clipboard(&cliprdr::data_response(Some(&bytes))).unwrap();
        assert_eq!(reply, None);
        let Some(Event::ClipboardData(data)) = event else { panic!("{event:?}") };
        assert_eq!(data, bytes);

        let (_, event) = answer_clipboard(&cliprdr::data_response(None)).unwrap();
        assert!(matches!(event, Some(Event::ClipboardRefused)));
    }

    /// The rest of MS-RDPECLIP, which this client asked for none of: nothing is
    /// said back and nobody is told, and in particular the session does not end.
    #[test]
    fn the_clipboard_pdus_this_client_asked_for_none_of_are_answered_with_nothing() {
        // A file contents request, and the response to a list this end sent.
        for pdu in [clipboard_pdu(0x0008, 0, &[0; 24]), clipboard_pdu(0x0003, 0x0002, &[])] {
            let (reply, event) = answer_clipboard(&pdu).unwrap();
            assert_eq!(reply, None);
            assert!(event.is_none(), "{event:?}");
        }
    }

    /// One clipboard PDU as a server writes one, for the two tests above that need a
    /// shape [`cliprdr`] does not write from this end.
    fn clipboard_pdu(kind: u16, flags: u16, body: &[u8]) -> Vec<u8> {
        let mut pdu = Vec::new();
        pdu.extend_from_slice(&kind.to_le_bytes());
        pdu.extend_from_slice(&flags.to_le_bytes());
        pdu.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        pdu.extend_from_slice(body);
        pdu
    }

    fn monitor_ready() -> Vec<u8> {
        clipboard_pdu(0x0001, 0, &[])
    }

    /// A channel the server closes takes its capabilities with it: what it said it
    /// would lay out was about a channel that no longer exists.
    #[test]
    fn closing_display_control_forgets_what_it_said_it_would_do() {
        let said = display::Capabilities { monitors: 1, area: 4 };
        let mut dynamics =
            Dynamics { resize: true, control: Some(11), caps: Some(said), ..Dynamics::default() };
        let elsewhere = dvc::Message::Close { channel: 12 };
        assert!(answer(elsewhere, &mut dynamics).unwrap().is_empty());
        assert_eq!(dynamics.control, Some(11), "another channel closing says nothing about this one");
        answer(dvc::Message::Close { channel: 11 }, &mut dynamics).unwrap();
        assert_eq!((dynamics.control, dynamics.caps), (None, None));
    }
}
