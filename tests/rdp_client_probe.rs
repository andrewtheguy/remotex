//! The RDP client against a real host, with the graphics pipeline or without it.
//!
//! No container stands in here: the client speaks NLA to a current Windows host and
//! nothing else, and what this exercises — the graphics pipeline's surfaces and
//! codecs, a graphics reset, or bitmap updates and a Deactivation-Reactivation
//! Sequence — is that host's behaviour. So, like `classify_render_e2e`, it
//! borrows a target from the operator's `tmp/test_uat.toml`, named by
//! [`TARGET_ENV`] rather than written here, and drives [`remotex::rdp_client`]
//! directly with no gateway in front of it.
//!
//! It connects, waits for a painted desktop, asks for a new size the way the
//! engine does — repeating the layout until the host answers, because a Windows
//! host ignores the first few seconds of them — puts the size back, takes the
//! remote clipboard over, and disconnects.
//!
//! What is asserted is the client's contract: a desktop arrives, gets painted,
//! and a requested size comes back as a resize of that size with a framebuffer to
//! match. Counts are printed, not asserted — they are the host's business.
//!
//! ## The graphics pipeline, measured
//!
//! [`EGFX_ENV`] chooses the path, and the pipeline is the default, as it is in the
//! gateway. Under it the client decodes every codec the sandbox sends — ClearCodec
//! with all three subcodecs, RemoteFX Progressive, planar and uncompressed — so the
//! probe asserts a lit desktop there as it does on the bitmap path, and after each
//! resize. Which codecs and commands the host chose is still the measurement, said at
//! the end of the session at `info`: run with `RUST_LOG=remotex=info` to read it, and
//! set [`DUMP_ENV`] to a directory to get the framebuffer as PNG at each stage, for
//! the check only eyes can make.
//!
//! ## The clipboard
//!
//! Both directions of MS-RDPECLIP are lazy — a copy announces which formats it can be
//! had in, and the bytes cost a second round trip that happens only when somebody
//! pastes — so the thing worth proving against a real host is that laziness, in both
//! directions, and the second test here does it: this end takes the remote clipboard
//! over, the remote pastes it and copies what it pasted, and the bytes that come back
//! are compared with the bytes that went out. See [`round_trip`], which drives the one
//! application every Windows desktop has to do it.
//!
//! The first test carries the clipboard channel without provoking it: it asserts the
//! host opens the channel and negotiates, answers any paste that happens to arrive,
//! and reports the rest. That the session survives all of it beside the desktop, the
//! pointer and two resizes is its own claim — a clipboard is a second static channel,
//! and a client that gets its chunk flags wrong loses the *other* channel rather than
//! this one.
//!
//! ## Sound
//!
//! The session asks for sound redirection and counts what arrives. A Windows host
//! negotiates nothing until something plays over there, so the negotiation is asserted
//! only under [`AUDIO_ENV`], set when a sound is playing on the remote; otherwise the
//! counts are printed and a quiet host is not a failure.
//!
//! ## The camera
//!
//! The session offers a camera and plugs one as soon as it connects, the way the
//! gateway does when a browser enables its camera. What a host does with it is the
//! host's: a Windows workstation opens the camera enumeration channel, agrees a
//! version and opens the device's channel once it is told about the device, and a
//! Windows Server without the Remote Desktop Session Host role opens nothing. So the
//! negotiation and the device channel are asserted only under [`CAMERA_ENV`], set
//! against a host that offers cameras; otherwise what happened is printed. Nothing
//! on the host opens the camera there, so no stream starts — run with
//! `RUST_LOG=remotex=debug` to read the host's device queries and this end's answers.
//!
//! ```sh
//! REMOTEX_UAT_TARGET=<rdp target in tmp/test_uat.toml> REMOTEX_UAT_AUDIO=1 REMOTEX_UAT_CAMERA=1 \
//!   cargo test --test rdp_client_probe -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The stream is the third test, [`stream_camera`], and it needs a stream to play and a
//! host with the Windows Camera app: it opens the app through the Run dialog, plays the
//! file named by [`CAMERA_STREAM_ENV`] into the camera the app opens, and closes the app
//! again. What it asserts is the host's part of the Video Capture sequence — it starts
//! the stream in the one format offered, takes the samples it asks for without stopping,
//! and stops when the app closes. Whether the picture in the app is the test pattern is
//! for eyes: take a screenshot of the remote while the samples play.
//!
//! ```sh
//! ffmpeg -f lavfi -i testsrc=size=640x480:rate=30 -t 20 -c:v libx264 -profile:v baseline \
//!   -pix_fmt yuv420p -g 30 -bf 0 -x264-params aud=1:repeat-headers=1 \
//!   -bsf:v h264_mp4toannexb -f h264 tmp/camera_probe_640x480.h264
//! REMOTEX_UAT_TARGET=<rdp target> REMOTEX_UAT_CAMERA_STREAM=tmp/camera_probe_640x480.h264 \
//!   cargo test --test rdp_client_probe a_real_host_streams_the_camera -- --ignored --nocapture
//! ```

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use remotex::rdp_client::proto::{rdpeai, rdpecam, rdpsnd};
use remotex::rdp_client::{AudioSink, Camera, CameraSink, Connect, Event, Fed, Input, MicrophoneSink, Session};
use remotex::rdp_clipboard::{self, CF_UNICODETEXT};
use tokio::sync::mpsc::Receiver;

/// Which target in `tmp/test_uat.toml` to drive — see the module docs.
const TARGET_ENV: &str = "REMOTEX_UAT_TARGET";

/// Whether to offer the graphics pipeline: anything but `0` or `false` does, and so
/// does leaving it unset.
const EGFX_ENV: &str = "REMOTEX_UAT_EGFX";

/// Whether a sound is playing on the remote for this run — set it to `1` when one is.
/// A Windows host sends its format list only once something plays, so the
/// negotiation is asserted only when this says it can be, and printed otherwise.
const AUDIO_ENV: &str = "REMOTEX_UAT_AUDIO";

/// Whether the target's host offers camera redirection — set it to `1` against a Windows
/// workstation, or a Server with the Remote Desktop Session Host role. The host's
/// negotiation and its opening of the device channel are asserted only when this says
/// it will, and printed otherwise.
const CAMERA_ENV: &str = "REMOTEX_UAT_CAMERA";

/// Whether the target's host offers microphone redirection — set it to `1` against a
/// Windows host that allows it. A policy can turn audio input off, so the negotiation
/// is asserted only when this says it will be, and printed otherwise.
const MICROPHONE_ENV: &str = "REMOTEX_UAT_MICROPHONE";

/// Whether the probe asks for sound at all: anything but `0` or `false` does. Off, it
/// shows that the host opens audio input without it — measured against a Windows
/// Enterprise host, it does.
const SOUND_ENV: &str = "REMOTEX_UAT_SOUND";

/// The camera the probe plugs: what a browser's webcam typically announces.
const PROBE_CAMERA: rdpecam::Format =
    rdpecam::Format { width: 640, height: 480, fps_numerator: 30, fps_denominator: 1 };

/// An Annex B H.264 file for [`stream_camera`] to play, in [`PROBE_CAMERA`]'s geometry
/// and rate, written with an access unit delimiter before every picture and the
/// parameter sets before every keyframe — the module docs have the ffmpeg line.
const CAMERA_STREAM_ENV: &str = "REMOTEX_UAT_CAMERA_STREAM";

/// What the Run dialog is given to open something that records: the Recording tab of
/// Sound, whose level meters open every capture device it lists.
const RECORDING_TAB: &str = "control mmsys.cpl,,1";

/// What the Run dialog is given to open the Windows Camera app.
const CAMERA_APP: &str = "microsoft.windows.camera:";

/// How long the samples play, and how far apart.
const CAMERA_FEED: Duration = Duration::from_secs(10);
const CAMERA_FRAME: Duration = Duration::from_micros(33_333);

/// The opening size, and the one each case asks to move to. Both even, both well
/// inside what any host accepts.
const OPENING: (u32, u32) = (1280, 800);
const RESIZED: (u32, u32) = (1600, 900);

/// How long a host gets to answer a layout before the case fails, and how often
/// the layout is repeated meanwhile.
const RESIZE_BUDGET: Duration = Duration::from_secs(30);
const RESIZE_RETRY: Duration = Duration::from_secs(2);

/// What this end puts on the remote's clipboard. Non-ASCII on purpose: the payload is
/// UTF-16 and a host that mangles it says so in the bytes that come back. One line,
/// because the round trip below pastes it into a single-line box on the remote.
const COPIED: &str = "remotex probe — 画面 ☕ round trip";

/// The scancodes the round trip needs. Driving the remote's own clipboard is the one
/// thing this end cannot do for itself, and a keystroke is all it has to do it with.
const LWIN: u8 = 0x5B;
const ESCAPE: u8 = 0x01;
const LCTRL: u8 = 0x1D;
const KEY_R: u8 = 0x13;
const KEY_A: u8 = 0x1E;
const KEY_C: u8 = 0x2E;
const KEY_V: u8 = 0x2F;
const ENTER: u8 = 0x1C;
const LALT: u8 = 0x38;
const F4: u8 = 0x3E;

/// Whether this run offers the graphics pipeline — see [`EGFX_ENV`].
fn egfx() -> bool {
    !matches!(std::env::var(EGFX_ENV).as_deref(), Ok("0") | Ok("false"))
}

/// Whether this run was told a sound is playing on the remote — see [`AUDIO_ENV`].
fn audio_playing() -> bool {
    matches!(std::env::var(AUDIO_ENV).as_deref(), Ok(v) if !matches!(v, "" | "0" | "false"))
}

/// Where the session's sound goes: counted, never played. What the host redirects
/// depends on what happens to be playing over there, so the numbers are printed
/// rather than asserted; the negotiation is asserted when [`AUDIO_ENV`] says
/// something is playing, which is the one condition under which a host does it.
#[derive(Default, Debug)]
struct Ear {
    negotiated: AtomicBool,
    buffers: AtomicU64,
    bytes: AtomicU64,
    closes: AtomicU64,
}

struct Listen(Arc<Ear>);

impl AudioSink for Listen {
    fn negotiated(&self, format: rdpsnd::Format) {
        assert_eq!(format, rdpsnd::CD_QUALITY);
        self.0.negotiated.store(true, Ordering::Relaxed);
    }

    fn wave(&self, samples: Vec<u8>) {
        self.0.buffers.fetch_add(1, Ordering::Relaxed);
        self.0.bytes.fetch_add(samples.len() as u64, Ordering::Relaxed);
    }

    fn closed(&self) {
        self.0.closes.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether this run was told the host offers cameras — see [`CAMERA_ENV`].
fn camera_offered() -> bool {
    matches!(std::env::var(CAMERA_ENV).as_deref(), Ok(v) if !matches!(v, "" | "0" | "false"))
}

/// What the host decided about the session's camera: counted, like the sound.
#[derive(Default, Debug)]
struct Eye {
    /// The version the host agreed, or 0 if it never did.
    version: AtomicU8,
    attached: AtomicBool,
    starts: AtomicU64,
    stops: AtomicU64,
    keyframes: AtomicU64,
}

struct Watch(Arc<Eye>);

impl CameraSink for Watch {
    fn negotiated(&self, version: u8) {
        self.0.version.store(version, Ordering::Relaxed);
    }

    fn attached(&self) {
        self.0.attached.store(true, Ordering::Relaxed);
    }

    fn started(&self, format: rdpecam::Format) {
        assert_eq!(format, PROBE_CAMERA, "the host started the camera in a format it was never offered");
        self.0.starts.fetch_add(1, Ordering::Relaxed);
    }

    fn stopped(&self) {
        self.0.stops.fetch_add(1, Ordering::Relaxed);
    }

    fn keyframe_needed(&self) {
        self.0.keyframes.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether this run was told the host offers a microphone — see [`MICROPHONE_ENV`].
fn microphone_offered() -> bool {
    matches!(std::env::var(MICROPHONE_ENV).as_deref(), Ok(v) if !matches!(v, "" | "0" | "false"))
}

/// Whether this run asks for sound — see [`SOUND_ENV`].
fn sound() -> bool {
    !matches!(std::env::var(SOUND_ENV).as_deref(), Ok("0") | Ok("false"))
}

/// What the host decided about the session's microphone: counted, like the sound.
#[derive(Default, Debug)]
struct Voice {
    /// The version the host agreed, or 0 if it never did.
    version: AtomicU64,
    /// The rate and channels of the last open, as `rate * 10 + channels`, or 0.
    opened: AtomicU64,
    opens: AtomicU64,
    closes: AtomicU64,
}

struct Speak(Arc<Voice>);

impl MicrophoneSink for Speak {
    fn negotiated(&self, version: u32) {
        self.0.version.store(u64::from(version), Ordering::Relaxed);
    }

    fn opened(&self, format: rdpeai::Format) {
        self.0.opened.store(u64::from(format.sample_rate) * 10 + u64::from(format.channels), Ordering::Relaxed);
        self.0.opens.fetch_add(1, Ordering::Relaxed);
    }

    fn closed(&self) {
        self.0.closes.fetch_add(1, Ordering::Relaxed);
    }
}

fn connect() -> (Session, Receiver<Event>, Arc<Ear>, Arc<Eye>) {
    let (session, events, ear, eye, _voice) = connect_with_voice();
    (session, events, ear, eye)
}

fn connect_with_voice() -> (Session, Receiver<Event>, Arc<Ear>, Arc<Eye>, Arc<Voice>) {
    let name = std::env::var(TARGET_ENV).unwrap_or_else(|_| {
        panic!("set {TARGET_ENV} to the name of an rdp target in tmp/test_uat.toml")
    });
    let target = common::uat_target(&name);
    println!("rdp_client_probe: {name} ({}:{}), egfx {}", target.host, target.port, egfx());
    let ear = Arc::new(Ear::default());
    let eye = Arc::new(Eye::default());
    let voice = Arc::new(Voice::default());
    let (session, events) = Session::start(Connect {
        host: target.host.clone(),
        port: target.port,
        username: target.username.clone(),
        password: target.password.clone(),
        domain: target.domain.clone(),
        width: OPENING.0,
        height: OPENING.1,
        resize: true,
        egfx: egfx(),
        clipboard: true,
        audio: sound().then(|| Box::new(Listen(Arc::clone(&ear))) as Box<dyn AudioSink>),
        camera: Some(Camera { name: "Remotex Probe Camera".to_owned(), sink: Box::new(Watch(Arc::clone(&eye))) }),
        microphone: Some(Box::new(Speak(Arc::clone(&voice)))),
    });
    (session, events, ear, eye, voice)
}

/// The microphone, recorded: the Audio Input sequence up to the host's Open, and PCM fed
/// in the packets it asked for.
///
/// A Windows host starts audio input only when something on it records (MS-RDPEAI
/// 3.1.4.1), the way it negotiates sound only when something plays. So the probe opens
/// the Recording tab of Sound through the Run dialog — its level meters open the
/// capture devices — waits for the host's open, feeds a few seconds of a tone in the
/// format it chose, and closes the dialog. What is asserted under [`MICROPHONE_ENV`] is
/// the negotiation; the rest is printed, and the session surviving all of it is the claim.
async fn record_microphone() {
    common::init_logging();
    let (session, mut events, _ear, _eye, voice) = connect_with_voice();
    let first = tokio::time::timeout(Duration::from_secs(60), events.recv())
        .await
        .expect("no first event within 60s")
        .expect("the event channel closed");
    assert!(matches!(first, Event::Connected { .. }), "the session did not connect: {first:?}");
    let microphone = session.microphone().expect("the session was given a microphone");

    let mut tally = Tally::default();
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(15), |t| {
        t.paints > 0 && t.clipboard_ready
    })
    .await;
    assert!(tally.clipboard_ready, "the host never opened its clipboard channel");
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(3), |_| false).await;
    println!("  sound {}, microphone after connect: {voice:?}", sound());

    // Something records: the Recording tab, opened as the Camera app is.
    let input = session.input();
    tally.offer = Some(RECORDING_TAB);
    input.advertise_clipboard(vec![CF_UNICODETEXT]);
    chord(input, &[(LWIN, true)], KEY_R, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false).await;
    chord(input, &[(LCTRL, false)], KEY_A, false);
    chord(input, &[(LCTRL, false)], KEY_V, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(4), |t| {
        !t.pastes.is_empty()
    })
    .await;
    chord(input, &[], ENTER, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(20), |_| {
        voice.opens.load(Ordering::Relaxed) > 0
    })
    .await;
    println!("  microphone with the Recording tab open: {voice:?}");

    let opened = voice.opened.load(Ordering::Relaxed);
    let mut fed = [0u64; 2]; // taken, refused
    if opened > 0 {
        let (rate, channels) = ((opened / 10) as usize, (opened % 10) as usize);
        let group = rate / 50; // 20 ms
        let mut phase = 0usize;
        let feeding = Instant::now();
        let mut next = feeding;
        while feeding.elapsed() < Duration::from_secs(4) {
            let mut pcm = Vec::with_capacity(group * channels * 2);
            for _ in 0..group {
                let value = ((phase as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16;
                phase += 1;
                for _ in 0..channels {
                    pcm.extend_from_slice(&value.to_le_bytes());
                }
            }
            fed[usize::from(!microphone.sample(pcm))] += 1;
            next += Duration::from_millis(20);
            pump(&session, &mut events, &mut tally, next, |_| false).await;
            // Mid-tone, the Recording tab's level meter for the redirected device is
            // what eyes can check the samples arrived by.
            if fed[0] == 150 {
                dump(&session, "microphone-recording");
            }
        }
    }
    // Close the Sound dialog, whatever happened above.
    chord(input, &[(LALT, false)], F4, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(3), |_| false).await;
    println!("  microphone: {voice:?}; buffers taken {}, refused {}", fed[0], fed[1]);
    if microphone_offered() {
        assert_eq!(
            voice.version.load(Ordering::Relaxed),
            u64::from(rdpeai::VERSION),
            "the host never agreed MS-RDPEAI version {}, though {MICROPHONE_ENV} says it offers \
             microphones",
            rdpeai::VERSION
        );
    }

    drop(session);
    let mut ended = None;
    while let Ok(event) = events.try_recv() {
        if let Event::Ended(result) = event {
            ended = Some(result);
        }
    }
    assert!(matches!(ended, Some(Ok(()))), "a disconnect this end asked for is an orderly end: {ended:?}");
}

/// What a stretch of the session did.
#[derive(Default, Debug)]
struct Tally {
    paints: u64,
    /// Frame boundaries the host marked, which only the graphics pipeline does.
    frames: u64,
    cursors: u64,
    resizes: Vec<(u32, u32)>,
    resize_ready: bool,
    /// The host opened its clipboard channel and started the negotiation.
    clipboard_ready: bool,
    /// What the remote clipboard announced, each time it announced something.
    remote_formats: Vec<Vec<u32>>,
    /// The formats the remote asked this end for — a paste over there, answered as
    /// it arrived.
    pastes: Vec<u32>,
    /// Text that came back from the remote clipboard, and the two ways it did not.
    remote_text: Vec<String>,
    refusals: u64,
    oversized: Vec<u64>,
    /// The text a paste on the remote is answered with, when not [`COPIED`].
    offer: Option<&'static str>,
}

/// Read events until `until` says stop or `deadline` passes. Panics on an ended
/// session: every case here expects the session to survive.
async fn pump(
    session: &Session,
    events: &mut Receiver<Event>,
    tally: &mut Tally,
    deadline: Instant,
    mut until: impl FnMut(&Tally) -> bool,
) -> bool {
    while !until(tally) {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = tokio::time::timeout(left, events.recv()).await else {
            return false;
        };
        match event.expect("the event channel closed without an Ended") {
            Event::Paint(_) => tally.paints += 1,
            Event::Frame => tally.frames += 1,
            Event::FramesMarked => {}
            Event::Cursor(_) => tally.cursors += 1,
            Event::Resize { width, height } => tally.resizes.push((width, height)),
            Event::ResizeReady { .. } => tally.resize_ready = true,
            Event::ResizeGone => tally.resize_ready = false,
            Event::ClipboardReady => tally.clipboard_ready = true,
            // Asked for at once, which is what the engine does and for the same
            // reason: a copy on the remote should be in hand before anybody asks.
            Event::ClipboardFormats(formats) => {
                if formats.contains(&CF_UNICODETEXT) {
                    session.input().request_clipboard(CF_UNICODETEXT);
                }
                tally.remote_formats.push(formats);
            }
            Event::ClipboardData(data) => {
                let text = rdp_clipboard::decode_unicode(&data)
                    .expect("the remote's clipboard text decodes as UTF-16");
                tally.remote_text.push(text);
            }
            Event::ClipboardRefused => tally.refusals += 1,
            Event::ClipboardOversized { bytes } => tally.oversized.push(bytes),
            // Answered here, and now: the application pasting on the far end is
            // blocked until this arrives. Answered the way the engine answers —
            // the one text format, encoded — so what the host reads back is what
            // the gateway would really hand it.
            Event::ClipboardWanted { format } => {
                tally.pastes.push(format);
                let data = (format == CF_UNICODETEXT)
                    .then(|| rdp_clipboard::encode_unicode(tally.offer.unwrap_or(COPIED)));
                session.input().send_clipboard(data);
            }
            Event::Connected { .. } => panic!("a second Connected"),
            Event::Ended(result) => panic!("the session ended: {result:?}"),
        }
    }
    true
}

/// Directory to write the framebuffer to as PNG at each stage, for eyes to check
/// what the counts cannot: that the decoded desktop looks like a desktop.
const DUMP_ENV: &str = "REMOTEX_UAT_DUMP";

/// Write the framebuffer as `<dir>/<name>.png` when [`DUMP_ENV`] names a directory.
fn dump(session: &Session, name: &str) {
    let Ok(dir) = std::env::var(DUMP_ENV) else { return };
    std::fs::create_dir_all(&dir).expect("creating the framebuffer dump directory");
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let bytes = session.framebuffer().with(|frame| {
        let mut rgba = frame.pixels.clone();
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 0xFF;
        }
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        writer.finish().expect("png finish");
        out
    });
    std::fs::write(&path, bytes).expect("writing the framebuffer dump");
    println!("  framebuffer written to {}", path.display());
}

/// How many of the framebuffer's pixels are not black: a desktop that decoded to
/// nothing — the classic failure of a codec that is not really working — is all 0.
fn lit(session: &Session) -> (u64, u64) {
    session.framebuffer().with(|frame| {
        let lit = frame.pixels.as_chunks::<4>().0.iter().filter(|px| px[..3] != [0, 0, 0]).count();
        (lit as u64, u64::from(frame.width) * u64::from(frame.height))
    })
}

/// Ask for `size` until the host resizes to it, the way the engine's retry ladder
/// does. Returns how long the host took.
async fn resize_to(
    session: &Session,
    events: &mut Receiver<Event>,
    tally: &mut Tally,
    size: (u32, u32),
) -> Duration {
    let started = Instant::now();
    let deadline = started + RESIZE_BUDGET;
    loop {
        session.input().resize(size.0, size.1, 100);
        let retry = (Instant::now() + RESIZE_RETRY).min(deadline);
        if pump(session, events, tally, retry, |t| t.resizes.last() == Some(&size)).await {
            return started.elapsed();
        }
        assert!(
            Instant::now() < deadline,
            "the host never resized to {}x{} (resizes seen: {:?})",
            size.0,
            size.1,
            tally.resizes
        );
    }
}

async fn case() {
    common::init_logging();
    let (session, mut events, ear, eye) = connect();

    let first = tokio::time::timeout(Duration::from_secs(60), events.recv())
        .await
        .expect("no first event within 60s")
        .expect("the event channel closed");
    let Event::Connected { width, height } = first else {
        panic!("the session did not connect: {first:?}");
    };
    println!("  connected at {width}x{height}");
    // Plugged as a browser's enable plugs it: once, early, before the host has opened
    // anything for it to be announced on.
    let camera = session.camera().expect("the session was given a camera");
    camera.plug(PROBE_CAMERA);

    // A desktop worth of drawing, and Display Control, before anything is resized.
    // Under the pipeline the drawing shows as frames; on the bitmap path, as paint.
    let egfx = egfx();
    let drawn = |t: &Tally| if egfx { t.frames > 0 } else { t.paints > 0 };
    let mut tally = Tally::default();
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(8), |t| {
        t.resize_ready && t.clipboard_ready && drawn(t)
    })
    .await;
    // And a moment more, so a desktop still arriving is counted whole.
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;
    let (on, total) = lit(&session);
    println!("  opening: {tally:?}, {on} of {total} pixels lit");
    dump(&session, "opening");
    assert!(drawn(&tally), "the host drew nothing");
    assert!(on > 0, "the desktop decoded to pure black");
    assert!(tally.resize_ready, "the host never offered Display Control");
    assert!(tally.clipboard_ready, "the host never opened its clipboard channel");

    for size in [RESIZED, OPENING] {
        let took = resize_to(&session, &mut events, &mut tally, size).await;
        // Let the repaint after the resize land.
        tally.paints = 0;
        tally.frames = 0;
        pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(3), |_| false)
            .await;
        let (on, total) = lit(&session);
        let framebuffer = session.framebuffer().with(|frame| (frame.width, frame.height));
        println!(
            "  resized to {}x{} after {:.1}s; {} paints and {} frames since, {on} of {total} \
             pixels lit",
            size.0,
            size.1,
            took.as_secs_f32(),
            tally.paints,
            tally.frames,
        );
        dump(&session, &format!("resized-{}x{}", size.0, size.1));
        assert_eq!(framebuffer, size, "the framebuffer did not follow the resize");
        assert!(drawn(&tally), "nothing was drawn after the resize");
        assert!(on > 0, "the resized desktop decoded to pure black");
    }

    // Typing and pointing must not upset the session: a few moves and a key that
    // changes nothing on a desktop (Shift), then a moment for any error to arrive.
    let input = session.input();
    for step in 0..20u16 {
        input.mouse_move(100 + step * 10, 100 + step * 5);
    }
    input.key(0x2A, false, true);
    input.key(0x2A, false, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;
    println!("  after input: {} cursor updates in all", tally.cursors);

    // The clipboard. Taking it over is this end's half of the bargain: one format
    // list, no bytes, and the host asks for those if and when anything over there
    // pastes. Whether it does is the host's business — Windows with clipboard
    // history on reads a newly owned clipboard itself — so what is *asserted* is
    // that the session survives the exchange, and that a paste that did arrive was
    // answered with the format it asked for.
    let before = tally.pastes.len();
    input.advertise_clipboard(vec![CF_UNICODETEXT]);
    // And ask the remote for whatever it holds. Nothing over there has copied
    // anything, and this end has just taken the clipboard over, so what comes back is
    // nothing at all — a Windows host does not answer a request for a format nobody
    // advertised, not even with the `CB_RESPONSE_FAIL` the specification allows. The
    // point of asking is that the session must not mind.
    input.request_clipboard(CF_UNICODETEXT);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(4), |_| false)
        .await;
    println!(
        "  clipboard: the remote announced {:?}, asked this end for {:?}, sent {:?} \
         ({} refusals, oversized {:?})",
        tally.remote_formats,
        tally.pastes,
        tally.remote_text,
        tally.refusals,
        tally.oversized,
    );
    for format in &tally.pastes[before..] {
        assert_eq!(*format, CF_UNICODETEXT, "the host asked for a format never offered");
    }
    println!("  sound: {ear:?}");
    if audio_playing() {
        assert!(
            ear.negotiated.load(Ordering::Relaxed),
            "the host never negotiated sound redirection, though {AUDIO_ENV} says a sound is \
             playing on the remote"
        );
    } else if !ear.negotiated.load(Ordering::Relaxed) {
        println!("  (no sound negotiated: a quiet host sends no format list; set {AUDIO_ENV}=1 with a sound playing to assert it)");
    }

    // The camera had the whole session to be negotiated and opened. Unplugging it is the
    // Device Removed Notification, and the session must survive the host's answer.
    println!("  camera: {eye:?}");
    if camera_offered() {
        assert_eq!(
            eye.version.load(Ordering::Relaxed),
            rdpecam::VERSION,
            "the host never agreed MS-RDPECAM version {}, though {CAMERA_ENV} says it offers \
             cameras",
            rdpecam::VERSION
        );
        assert!(
            eye.attached.load(Ordering::Relaxed),
            "the host never opened the announced camera's device channel"
        );
    } else if eye.version.load(Ordering::Relaxed) == 0 {
        println!("  (no camera negotiated: set {CAMERA_ENV}=1 against a host that offers cameras to assert it)");
    }
    camera.unplug();
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;

    drop(session);
    // The drop disconnected and joined the thread, so its last word is here.
    let mut ended = None;
    while let Ok(event) = events.try_recv() {
        if let Event::Ended(result) = event {
            ended = Some(result);
        }
    }
    println!("  ended: {ended:?}");
    assert!(matches!(ended, Some(Ok(()))), "a disconnect this end asked for is an orderly end");
}

/// Tap one key, with modifiers held around it.
///
/// Held and released here rather than left down: a modifier this end forgets is one
/// the remote desktop keeps, and every keystroke after it — including a person's,
/// later — arrives wearing it.
fn chord(input: &Input, modifiers: &[(u8, bool)], key: u8, extended: bool) {
    for &(scancode, ext) in modifiers {
        input.key(scancode, ext, true);
    }
    input.key(key, extended, true);
    input.key(key, extended, false);
    for &(scancode, ext) in modifiers.iter().rev() {
        input.key(scancode, ext, false);
    }
}

/// The clipboard, all the way around and back.
///
/// The half this end cannot do for itself is the remote's: something over there has to
/// paste, and something has to copy. The Run dialog is the application every Windows
/// desktop has — Win+R opens it, Ctrl+V pastes into its one-line box, Ctrl+A and
/// Ctrl+C copy that back out, and Escape closes it — so the whole round trip is five
/// keystrokes and no typing. Nothing is entered and no Enter is sent, so the dialog
/// leaves the desktop as it found it.
///
/// Both halves of the protocol's laziness are what this proves:
///
/// - the paste makes the host ask this end to render the text it advertised, which is
///   delayed rendering in the direction that matters — a request left unanswered is an
///   application over there stopped inside its own paste;
/// - the copy makes the host announce a format list, which this end asks for and
///   reads, and the bytes that come back are the bytes that went out.
async fn round_trip() {
    common::init_logging();
    let (session, mut events, _ear, _eye) = connect();
    let first = tokio::time::timeout(Duration::from_secs(60), events.recv())
        .await
        .expect("no first event within 60s")
        .expect("the event channel closed");
    assert!(matches!(first, Event::Connected { .. }), "the session did not connect: {first:?}");

    let mut tally = Tally::default();
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(10), |t| {
        t.clipboard_ready && t.paints > 0
    })
    .await;
    assert!(tally.clipboard_ready, "the host never opened its clipboard channel");

    // 1. Take the remote clipboard over. Nothing is transferred: the host now knows
    //    this end holds text, and will ask for it if anything pastes.
    let input = session.input();
    input.advertise_clipboard(vec![CF_UNICODETEXT]);

    // 2. Open the Run dialog and paste into it. The dialog needs a moment to exist
    //    and take focus before the paste can land in it.
    chord(input, &[(LWIN, true)], KEY_R, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;
    // Select what the dialog pre-filled itself with before pasting over it: it opens
    // holding whatever was last run on that desktop, and a paste that landed beside
    // it would be read back as somebody else's text with this end's appended.
    chord(input, &[(LCTRL, false)], KEY_A, false);
    chord(input, &[(LCTRL, false)], KEY_V, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(4), |t| {
        !t.pastes.is_empty()
    })
    .await;
    assert_eq!(
        tally.pastes,
        vec![CF_UNICODETEXT],
        "the remote never asked this end to render what it had advertised; the Run dialog \
         may not have taken the paste"
    );

    // 3. Select what was pasted and copy it, which hands the clipboard back to the
    //    remote — and this end asks for it the moment the format list arrives.
    chord(input, &[(LCTRL, false)], KEY_A, false);
    chord(input, &[(LCTRL, false)], KEY_C, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(6), |t| {
        !t.remote_text.is_empty()
    })
    .await;

    // 4. Close the dialog, whatever happened above, before anything is asserted: an
    //    open dialog left on the desktop is this test's litter.
    chord(input, &[], ESCAPE, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;

    println!(
        "  round trip: pasted {:?}, the remote then announced {:?} and sent {:?} \
         ({} refusals, oversized {:?})",
        tally.pastes, tally.remote_formats, tally.remote_text, tally.refusals, tally.oversized,
    );
    let announced = tally.remote_formats.last().expect("the remote announced its copy");
    assert!(announced.contains(&CF_UNICODETEXT), "the copy carried no Unicode text: {announced:?}");
    assert_eq!(
        tally.remote_text,
        vec![COPIED.to_owned()],
        "what came back off the remote clipboard is not what went out"
    );

    drop(session);
    let mut ended = None;
    while let Ok(event) = events.try_recv() {
        if let Event::Ended(result) = event {
            ended = Some(result);
        }
    }
    assert!(matches!(ended, Some(Ok(()))), "a disconnect this end asked for is an orderly end");
}

/// The access units of an Annex B stream written with access unit delimiters, each with
/// whether it holds an IDR picture.
fn access_units(stream: &[u8]) -> Vec<(Vec<u8>, bool)> {
    let starts: Vec<usize> = (0..stream.len().saturating_sub(4))
        .filter(|&at| stream[at..at + 4] == [0, 0, 0, 1] && stream[at + 4] & 0x1F == 9)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &at)| {
            let unit = &stream[at..starts.get(n + 1).copied().unwrap_or(stream.len())];
            let idr = unit.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1F == 5);
            (unit.to_vec(), idr)
        })
        .collect()
}

/// The camera, streaming: the Video Capture sequence a real application on the host
/// starts, fed real H.264.
///
/// The Windows Camera app is the application every Windows desktop has, and the Run
/// dialog opens it by its protocol name — pasted, because a keystroke per character is
/// a keyboard layout's business. The app opens the only camera there is, which is this
/// one; the host starts the stream, and the samples play at the stream's rate for
/// [`CAMERA_FEED`], with events drained between them as the gateway drains them. Alt+F4
/// closes the app, which is the host's cue to stop.
///
/// Measured against a Windows Enterprise host, the app tears its first stream down a few
/// seconds in — Deactivate, and every device channel closed — then opens the device again
/// and starts a second stream that runs until the app closes. So what is asserted is that
/// a stream was running when the samples ran out, not that none stopped before.
async fn stream_camera() {
    common::init_logging();
    let path = std::env::var(CAMERA_STREAM_ENV).unwrap_or_else(|_| {
        panic!("set {CAMERA_STREAM_ENV} to an Annex B H.264 file at 640x480, 30 frames a second")
    });
    let units = access_units(&std::fs::read(&path).expect("reading the camera stream"));
    assert!(units.first().is_some_and(|(_, idr)| *idr), "the stream does not open on a keyframe");
    println!("  {} access units from {path}", units.len());

    let (session, mut events, _ear, eye) = connect();
    let first = tokio::time::timeout(Duration::from_secs(60), events.recv())
        .await
        .expect("no first event within 60s")
        .expect("the event channel closed");
    assert!(matches!(first, Event::Connected { .. }), "the session did not connect: {first:?}");
    let camera = session.camera().expect("the session was given a camera");
    camera.plug(PROBE_CAMERA);

    let mut tally = Tally::default();
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(20), |t| {
        t.clipboard_ready && t.paints > 0 && eye.attached.load(Ordering::Relaxed)
    })
    .await;
    assert!(tally.clipboard_ready, "the host never opened its clipboard channel");
    assert!(eye.attached.load(Ordering::Relaxed), "the host never opened the camera's device channel");
    // A desktop a moment old is still being rebuilt — a reclaimed session resets its
    // graphics here — and a keystroke that lands before it settles opens nothing.
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(3), |_| false)
        .await;

    // 1. Open the Camera app.
    let input = session.input();
    tally.offer = Some(CAMERA_APP);
    input.advertise_clipboard(vec![CF_UNICODETEXT]);
    chord(input, &[(LWIN, true)], KEY_R, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;
    chord(input, &[(LCTRL, false)], KEY_A, false);
    chord(input, &[(LCTRL, false)], KEY_V, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(4), |t| {
        !t.pastes.is_empty()
    })
    .await;
    // Not asserted: the Run dialog remembers what it last ran, and a paste it did not ask
    // this end for still opens the app. Whether the host starts a stream is the check.
    println!("  the Run dialog asked this end for {:?}", tally.pastes);
    chord(input, &[], ENTER, false);
    let started = pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(30), |_| {
        eye.starts.load(Ordering::Relaxed) > 0
    })
    .await;

    // 2. Play the stream into it.
    let mut fed = [0u64; 4]; // queued, dropped, skipped, ended
    if started {
        let feeding = Instant::now();
        let mut next = feeding;
        for (unit, idr) in units.iter().cycle() {
            if feeding.elapsed() >= CAMERA_FEED {
                break;
            }
            fed[match camera.sample(unit, *idr) {
                Fed::Queued => 0,
                Fed::Dropped => 1,
                Fed::Skipped => 2,
                Fed::Ended => 3,
            }] += 1;
            next += CAMERA_FRAME;
            pump(&session, &mut events, &mut tally, next, |_| false).await;
        }
    }
    let stops_while_playing = eye.stops.load(Ordering::Relaxed);
    let playing = eye.starts.load(Ordering::Relaxed) > stops_while_playing;
    // What the app shows of the samples is for eyes, as a desktop's picture is.
    dump(&session, "camera-playing");

    // 3. Close the app, whatever happened above, and give the host its moment to stop.
    chord(input, &[(LALT, false)], F4, false);
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(10), |_| {
        eye.stops.load(Ordering::Relaxed) > stops_while_playing
    })
    .await;
    // Whether the app is gone, or something of it is still up, is for eyes too.
    dump(&session, "camera-closed");
    println!(
        "  camera stream: {eye:?}; samples queued {}, dropped {}, skipped {}, after the end {}",
        fed[0], fed[1], fed[2], fed[3]
    );
    camera.unplug();
    pump(&session, &mut events, &mut tally, Instant::now() + Duration::from_secs(2), |_| false)
        .await;

    assert!(started, "the host never started the camera; the Camera app may not have opened");
    assert!(fed[0] > 0, "no sample reached the session");
    assert_eq!(fed[3], 0, "the session ended while the samples played");
    assert!(playing, "no stream was running when the samples ran out");
    assert!(
        eye.stops.load(Ordering::Relaxed) > stops_while_playing,
        "closing the Camera app did not stop the stream"
    );

    drop(session);
    let mut ended = None;
    while let Ok(event) = events.try_recv() {
        if let Event::Ended(result) = event {
            ended = Some(result);
        }
    }
    assert!(matches!(ended, Some(Ok(()))), "a disconnect this end asked for is an orderly end");
}

#[tokio::test]
#[ignore = "drives a real RDP host named in tmp/test_uat.toml"]
async fn a_real_host_paints_and_resizes() {
    case().await;
}

#[tokio::test]
#[ignore = "drives a real RDP host named in tmp/test_uat.toml, and its Run dialog"]
async fn a_real_host_round_trips_the_clipboard() {
    round_trip().await;
}

#[tokio::test]
#[ignore = "drives a real RDP host named in tmp/test_uat.toml, its Camera app, and a stream in REMOTEX_UAT_CAMERA_STREAM"]
async fn a_real_host_streams_the_camera() {
    stream_camera().await;
}

#[tokio::test]
#[ignore = "drives a real RDP host named in tmp/test_uat.toml"]
async fn a_real_host_records_the_microphone() {
    record_microphone().await;
}
