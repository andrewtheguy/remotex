//! End-to-end test of the VNC engine against wlshare, the one generic VNC server
//! that answers the gateway's private density and outputs extensions.
//!
//! Builds wlshare from its checkout — `../wlshare` beside this repository, or
//! wherever `REMOTEX_TEST_WLSHARE_DIR` names — and starts it on a headless sway
//! (`tests/wlshare-dummy/`) with two outputs, HEADLESS-1 at 1024x768 and scale 1
//! and HEADLESS-2 at 1280x800 and scale 2, then drives a session through the
//! gateway as a 2x browser would. Every step is
//! asserted twice: on what the browser is told, and on what sway says the output
//! became, so a gateway that reports a density or a size the server never
//! applied fails here.
//!
//! The same container runs PipeWire, so the browser's microphone and camera are
//! followed too: Opus and H.264 go in on their sockets, and what an application on
//! the desktop takes from wlshare's nodes comes out.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remotex::config::{AppConfig, Protocol, TargetConfig};
use remotex::server;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

const TARGET: &str = "wlshare-dummy";

/// Where the wlshare checkout to build is, when not beside this repository.
const WLSHARE_DIR_ENV: &str = "REMOTEX_TEST_WLSHARE_DIR";

/// Wait until wlshare answers RFB on the published port: rootless podman's
/// forwarder accepts a connection before anything listens inside.
async fn wait_for_vnc_port(port: u16) {
    use tokio::io::AsyncReadExt as _;

    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let attempt = async {
                let mut stream = TcpStream::connect((common::container_host(), port)).await.ok()?;
                let mut greeting = [0u8; 12];
                stream.read_exact(&mut greeting).await.ok()?;
                greeting.starts_with(b"RFB ").then_some(())
            };
            match tokio::time::timeout(Duration::from_secs(2), attempt).await {
                Ok(Some(())) => return,
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    })
    .await
    .expect("wlshare never sent an RFB greeting");
}

/// Start the real server pointed at wlshare: a plain `vnc` target with
/// `resize`, a camera and a microphone, and nothing naming the server — the
/// extensions are discovered.
async fn spawn_app(vnc_port: u16) -> SocketAddr {
    let config = AppConfig {
        listen: remotex::config::ListenAddr::Tcp("127.0.0.1:0".to_owned()),
        auth: common::test_auth(),
        branding: remotex::config::Branding { text: "remotex".to_owned(), logo: None },
        dev_hostname: None,
        meter: None,
        targets: vec![TargetConfig {
            name: TARGET.to_owned(),
            protocol: Protocol::Vnc,
            subtype: None,
            host: common::container_host(),
            port: vnc_port,
            // The container's wlshare asks for no login.
            username: String::new(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: None,
            height: None,
            resize: true,
            egfx: None,
            clipboard: false,
            audio: false,
            audio_codec: None,
            camera: true,
            microphone: true,
            render_type: remotex::config::RenderType::Tiles,
            render_subtype: None,
            render_stream_quality: None,
            render_subtype_quality: None,
            render_motion: false,
            render_motion_debug: false,
            render_chroma: None,
            render_classify_debug: false,
            render_grid_debug: false,
            render_adaptive: false,
            render_adaptive_min: None,
            audio_bitrate: None,
            audio_adaptive: false,
            audio_bitrate_min: None,
        }],
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config, Default::default());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// A `resize` control message: the framebuffer's pixels and their density.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Size {
    w: u64,
    h: u64,
    scale: f64,
}

/// What the browser has been told so far on one socket.
struct View {
    stream: common::TileStream,
    /// The last announced size; tiles are bounded by it.
    size: Option<Size>,
    /// Every size announced, in order.
    sizes: Vec<Size>,
    /// The last `displays` message: the active id and each entry's id and label.
    displays: Option<(u64, Vec<(u64, String)>)>,
    /// Pixels painted since the last announced size.
    covered: u64,
}

impl View {
    fn new() -> Self {
        Self {
            stream: common::TileStream::new(),
            size: None,
            sizes: Vec::new(),
            displays: None,
            covered: 0,
        }
    }

    /// Read until `done` holds for this view with the framebuffer fully painted
    /// at the last announced size, failing on a session error or a tile outside
    /// the desktop it arrived under.
    async fn until(&mut self, ws: &mut common::Ws, what: &str, done: impl Fn(&Self) -> bool) {
        let painted = |view: &Self| {
            view.size.is_some_and(|size| view.covered >= size.w * size.h) && done(view)
        };
        tokio::time::timeout(Duration::from_secs(60), async {
            while !painted(self) {
                let Some(msg) = ws.next().await else {
                    panic!("websocket closed waiting for {what}");
                };
                match msg.expect("websocket receive") {
                    Message::Text(text) => self.control(&text),
                    Message::Binary(frame) => self.tiles(&frame),
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}: sizes {:?}, displays {:?}", self.sizes, self.displays));
    }

    fn control(&mut self, text: &str) {
        let msg: serde_json::Value = serde_json::from_str(text).expect("control message is JSON");
        match msg["type"].as_str() {
            Some("error") => panic!("session failed: {text}"),
            Some("resize") => {
                let size = Size {
                    w: msg["w"].as_u64().unwrap(),
                    h: msg["h"].as_u64().unwrap(),
                    scale: msg["scale"].as_f64().unwrap(),
                };
                self.size = Some(size);
                self.sizes.push(size);
                self.covered = 0;
            }
            Some("displays") => {
                let entries = msg["displays"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|d| (d["id"].as_u64().unwrap(), d["label"].as_str().unwrap().to_owned()))
                    .collect();
                self.displays = Some((msg["active"].as_u64().unwrap(), entries));
            }
            _ => {}
        }
    }

    fn tiles(&mut self, frame: &[u8]) {
        let size = self.size.expect("a tile arrived before any resize");
        for record in self.stream.paint(frame) {
            let (x, y, w, h) = record.rect();
            assert!(
                u64::from(x) + u64::from(w) <= size.w && u64::from(y) + u64::from(h) <= size.h,
                "rectangle {w}x{h}+{x}+{y} exceeds the {}x{} desktop",
                size.w,
                size.h
            );
            self.covered += u64::from(w) * u64::from(h);
        }
    }

    fn output(&self, name: &str) -> u64 {
        let (_, entries) = self.displays.as_ref().expect("no displays message yet");
        entries
            .iter()
            .find(|(_, label)| label == name)
            .unwrap_or_else(|| panic!("{name} is not in the display list {entries:?}"))
            .0
    }

    fn active(&self) -> Option<u64> {
        self.displays.as_ref().map(|(active, _)| *active)
    }
}

/// An output as sway reports it: its current mode and scale.
fn sway_output(container: &common::Container, name: &str) -> (u64, u64, f64) {
    let json = container.exec(&[
        "sh",
        "-c",
        "SWAYSOCK=$(ls /tmp/xdg/sway-ipc.*.sock) swaymsg -r -t get_outputs",
    ]);
    let outputs: serde_json::Value = serde_json::from_str(&json).expect("swaymsg output is JSON");
    let output = outputs
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == name)
        .unwrap_or_else(|| panic!("sway has no output {name}: {json}"));
    let mode = &output["current_mode"];
    (
        mode["width"].as_u64().unwrap(),
        mode["height"].as_u64().unwrap(),
        output["scale"].as_f64().unwrap(),
    )
}

#[tokio::test]
#[ignore = "requires Docker or Podman"]
async fn wlshare_follows_the_browsers_density_size_and_output() {
    common::init_logging();
    let (container, vnc_port) = start_wlshare().await;

    let addr = spawn_app(vnc_port).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    let mut view = View::new();

    // A 2x browser opens the session. wlshare reports HEADLESS-1 at scale 1, and
    // the gateway declares 2x with the desktop's 1024x768 points in pixels at
    // 2x: the output's mode and scale change in one configuration, and the
    // logical size stays what it was.
    ws.send(Message::text(format!(
        r#"{{"type":"connect","target":"{TARGET}","display":{{"w":1728,"h":1117,"scale":200}}}}"#
    )))
    .await
    .unwrap();
    let doubled = Size { w: 2048, h: 1536, scale: 2.0 };
    view.until(&mut ws, "HEADLESS-1 at 2x, its logical size kept", |v| {
        v.size == Some(doubled) && v.displays.is_some()
    })
    .await;
    assert_eq!(
        view.sizes.first().map(|s| s.scale),
        Some(1.0),
        "the output's own scale comes first: {:?}",
        view.sizes
    );
    let first = view.output("HEADLESS-1");
    let second = view.output("HEADLESS-2");
    assert_eq!(view.active(), Some(first), "the configured output is shared first");
    assert_eq!(sway_output(&container, "HEADLESS-1"), (2048, 1536, 2.0));

    // The window's points are asked for in the output's pixels.
    ws.send(Message::text(r#"{"type":"viewport","w":800,"h":600}"#)).await.unwrap();
    let window = Size { w: 1600, h: 1200, scale: 2.0 };
    view.until(&mut ws, "the viewport at 2x", |v| v.size == Some(window)).await;
    assert_eq!(sway_output(&container, "HEADLESS-1"), (1600, 1200, 2.0));

    // Switch outputs. The checkmark moves when wlshare says it moved, the
    // desktop becomes HEADLESS-2's own 1280x800, and the window follows there.
    let before = view.sizes.len();
    ws.send(Message::text(format!(r#"{{"type":"selectDisplay","id":{second}}}"#)))
        .await
        .unwrap();
    view.until(&mut ws, "the window on HEADLESS-2", |v| {
        v.active() == Some(second)
            && v.size == Some(window)
            && v.sizes[before..].iter().any(|s| *s != window)
    })
    .await;
    assert_eq!(sway_output(&container, "HEADLESS-2"), (1600, 1200, 2.0));
    assert_eq!(
        sway_output(&container, "HEADLESS-1"),
        (1600, 1200, 2.0),
        "the output left behind keeps what it was set to"
    );
}

/// How long the desktop's application records the microphone.
const RECORD_SECS: u32 = 4;

/// The tone the browser's microphone hears.
const TONE_HZ: f64 = 440.0;

/// Build and start the wlshare container, and wait until it answers RFB.
async fn start_wlshare() -> (Arc<common::Container>, u16) {
    let runtime = common::container_runtime();
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let wlshare = std::env::var_os(WLSHARE_DIR_ENV)
        .map_or_else(|| manifest.join("../wlshare"), std::path::PathBuf::from);
    assert!(
        wlshare.join("crates/wlshare").is_dir(),
        "no wlshare checkout at {}; clone it beside this repository or set {WLSHARE_DIR_ENV}",
        wlshare.display()
    );
    let (container, vnc_port) = common::start_server_image(
        runtime,
        "remotex-e2e-wlshare",
        &manifest.join("tests/wlshare-dummy"),
        &wlshare,
        5900,
    );
    wait_for_vnc_port(vnc_port).await;
    (Arc::new(container), vnc_port)
}

/// Run a shell script in the container off the runtime's thread, which the gateway
/// under test shares. The future owns its container, so it can be spawned.
fn exec(container: &Arc<common::Container>, script: String) -> impl Future<Output = String> + 'static {
    let container = Arc::clone(container);
    async move {
        tokio::task::spawn_blocking(move || container.exec(&["sh", "-c", &script]))
            .await
            .expect("container exec task")
    }
}

/// The name of the wlshare node PipeWire lists under `prefix`, if it lists one:
/// `wlshare-microphone-` or `wlshare-camera-`.
async fn pipewire_node(container: &Arc<common::Container>, prefix: &str) -> Option<String> {
    let nodes = exec(container, "XDG_RUNTIME_DIR=/tmp/xdg pw-cli ls Node".to_owned()).await;
    nodes.lines().find_map(|line| {
        let name = line.trim().strip_prefix("node.name = \"")?.strip_suffix('"')?;
        name.starts_with(prefix).then(|| name.to_owned())
    })
}

/// Poll until the node under `prefix` is there (`Some`) or gone (`None`).
async fn until_node(container: &Arc<common::Container>, prefix: &str, present: bool) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let node = pipewire_node(container, prefix).await;
            if node.is_some() == present {
                return node;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {prefix}* node became {}", if present { "present" } else { "absent" }))
}

/// The next control message on an uplink socket, whose only text is the remote's
/// decisions.
async fn uplink_signal(ws: &mut common::Ws) -> serde_json::Value {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).expect("uplink signal is JSON"),
            Some(Ok(_)) => {}
            other => panic!("the uplink socket ended: {other:?}"),
        }
    }
}

/// Connect to the target and read the session socket up to `connected`, which is
/// returned; after it the socket is only drained, so nothing backs up behind it.
async fn open_session(ws: &mut common::Ws) -> serde_json::Value {
    common::connect_target(ws, TARGET).await;
    loop {
        if let Message::Text(text) = ws.next().await.expect("session socket open").expect("websocket receive") {
            let msg: serde_json::Value = serde_json::from_str(&text).expect("control message is JSON");
            match msg["type"].as_str() {
                Some("connected") => return msg,
                Some("error") => panic!("session failed: {text}"),
                _ => {}
            }
        }
    }
}

/// 60 ms Opus packets of a continuous tone, mono at 48 kHz in voice mode — what the
/// browser's encoder sends — each framed as the mic socket takes it.
fn tone_frames(count: usize) -> Vec<Vec<u8>> {
    let mut encoder = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    encoder.set_bitrate(opus::Bitrate::Bits(16_000)).unwrap();
    (0..count)
        .map(|packet| {
            let pcm: Vec<f32> = (0..2880)
                .map(|n| {
                    let t = (packet * 2880 + n) as f64 / 48_000.0;
                    ((t * TONE_HZ * std::f64::consts::TAU).sin() * 0.5) as f32
                })
                .collect();
            let mut frame = vec![remotex::protocol::mic::FRAME_KIND];
            frame.extend(encoder.encode_vec_float(&pcm, 4000).unwrap());
            frame
        })
        .collect()
}

/// The power of 48 kHz `samples` at `hz`, by Goertzel's algorithm.
fn power_at(samples: &[f64], hz: f64) -> f64 {
    let coefficient = 2.0 * (std::f64::consts::TAU * hz / 48_000.0).cos();
    let (mut q1, mut q2) = (0.0, 0.0);
    for sample in samples {
        let q0 = coefficient * q1 - q2 + sample;
        q2 = q1;
        q1 = q0;
    }
    q1 * q1 + q2 * q2 - coefficient * q1 * q2
}

/// The browser enables its microphone the moment the session starts, while the
/// gateway is still connecting to wlshare, and sends a tone. wlshare lends the
/// desktop a node; an application recording from it is what starts the browser's
/// audio and what stops it, and what it records is the tone. Closing the socket takes
/// the node away.
#[tokio::test]
#[ignore = "requires Docker or Podman"]
async fn wlshare_lends_the_desktop_the_browsers_microphone() {
    use base64::Engine as _;

    common::init_logging();
    let (container, vnc_port) = start_wlshare().await;
    let addr = spawn_app(vnc_port).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    let connected = open_session(&mut ws).await;
    assert_eq!(connected["microphone"], true, "the target carries a microphone: {connected}");

    // `connected` is sent as the engine starts, ahead of its RFB handshake, so the
    // microphone is plugged before the engine can have heard wlshare's answer. How far
    // ahead is the handshake's to decide; `MicBridge`'s unit tests fix the order.
    let mut mic = common::connect_mic_ws(addr, &token, &cookie).await;
    // The session's tiles are nobody's business here, but its socket is read so
    // nothing backs up behind it.
    let session = tokio::spawn(async move { while let Some(Ok(_)) = ws.next().await {} });

    let node = until_node(&container, "wlshare-microphone-", true).await.expect("a node");
    let recording = tokio::spawn(exec(
        &container,
        format!(
            // `--raw` writes to stdout whatever file it is named.
            "XDG_RUNTIME_DIR=/tmp/xdg timeout {RECORD_SECS} pw-record --raw --target {node} \
             --rate 48000 --channels 1 --format s16 - | base64 -w0"
        ),
    ));
    assert_eq!(uplink_signal(&mut mic).await["type"], "micOpen", "recording opens the browser's microphone");

    // Sent in real time until the recording's end closes the microphone.
    let frames = tone_frames(50);
    let mut tick = tokio::time::interval(Duration::from_millis(60));
    let mut sent = 0;
    tokio::time::timeout(Duration::from_secs(u64::from(RECORD_SECS) + 20), async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    mic.send(Message::binary(frames[sent % frames.len()].clone())).await.unwrap();
                    sent += 1;
                }
                signal = uplink_signal(&mut mic) => {
                    assert_eq!(signal["type"], "micClose", "the recording's end closes the browser's microphone");
                    break;
                }
            }
        }
    })
    .await
    .expect("the recording never ended");

    let recording = recording.await.expect("recording task");
    let raw = base64::engine::general_purpose::STANDARD.decode(recording.trim()).expect("base64 recording");
    let samples: Vec<f64> = raw.as_chunks::<2>().0.iter().map(|s| f64::from(i16::from_le_bytes(*s))).collect();
    // What the desktop heard of the tone, past the silence before the first packet: the
    // 20 ms blocks with anything in them.
    let heard: Vec<f64> = samples
        .as_chunks::<960>()
        .0
        .iter()
        .filter(|block| (block.iter().map(|s| s * s).sum::<f64>() / 960.0).sqrt() > 1_000.0)
        .flatten()
        .copied()
        .collect();
    assert!(
        heard.len() >= 48_000,
        "{} of {} recorded samples carry sound after {sent} packets",
        heard.len(),
        samples.len()
    );
    let loudest = (300..=700)
        .step_by(10)
        .map(f64::from)
        .max_by(|a, b| power_at(&heard, *a).total_cmp(&power_at(&heard, *b)))
        .unwrap();
    assert_eq!(loudest, TONE_HZ, "the desktop records the browser's tone");

    mic.close(None).await.unwrap();
    until_node(&container, "wlshare-microphone-", false).await;
    session.abort();
}

/// The camera fixture: two seconds of libx264 Constrained Baseline at 320x240 and
/// 15/1, a keyframe every fifteen frames, in Annex B.
const CAMERA_H264: &[u8] = include_bytes!("fixtures/camera-320x240-15fps.h264");

/// How many pictures the desktop's application takes from the camera.
const CAMERA_BUFFERS: u32 = 10;

/// One H.264 access unit and whether it is a keyframe.
struct AccessUnit {
    data: Vec<u8>,
    keyframe: bool,
}

/// Split an Annex B stream into the access units a browser's encoder hands over
/// one at a time. A unit starts at a parameter set or SEI that follows a picture's
/// slices, or at a slice whose first macroblock is 0 that follows another slice —
/// the one boundary a stream of several slices per picture makes visible.
fn access_units(stream: &[u8]) -> Vec<AccessUnit> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 <= stream.len() {
        if stream[at..].starts_with(&[0, 0, 1]) {
            let begin = if at > 0 && stream[at - 1] == 0 { at - 1 } else { at };
            starts.push((begin, at + 3));
            at += 3;
        } else {
            at += 1;
        }
    }
    let mut units: Vec<AccessUnit> = Vec::new();
    let mut previous_was_slice = false;
    for (n, &(begin, header)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(stream.len(), |next| next.0);
        let kind = stream[header] & 0x1f;
        let slice = kind == 1 || kind == 5;
        // A first_mb_in_slice of 0 is ue(v) "1": the payload's first bit set.
        let first_slice = slice && stream[header + 1] & 0x80 != 0;
        let starts_unit = units.is_empty() || previous_was_slice && (!slice || first_slice);
        if starts_unit {
            units.push(AccessUnit { data: Vec::new(), keyframe: false });
        }
        let unit = units.last_mut().expect("a unit");
        unit.data.extend_from_slice(&stream[begin..end]);
        unit.keyframe |= kind == 5;
        previous_was_slice = slice;
    }
    units
}

#[test]
fn the_camera_fixture_is_thirty_pictures_with_a_keyframe_every_fifteen() {
    let units = access_units(CAMERA_H264);
    assert_eq!(units.len(), 30);
    let keyframes: Vec<usize> = units.iter().enumerate().filter(|(_, u)| u.keyframe).map(|(n, _)| n).collect();
    assert_eq!(keyframes, [0, 15]);
}

/// The browser enables its camera the moment the session starts, while the gateway
/// is still connecting to wlshare, and announces 320x240 at 15/1. wlshare lends the
/// desktop a node; an application opening it is what starts the browser's pictures
/// and what stops them, and it takes whole pictures of the plugged size. Closing the
/// socket takes the node away.
#[tokio::test]
#[ignore = "requires Docker or Podman"]
async fn wlshare_lends_the_desktop_the_browsers_camera() {
    common::init_logging();
    let (container, vnc_port) = start_wlshare().await;
    let addr = spawn_app(vnc_port).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    let connected = open_session(&mut ws).await;
    assert_eq!(connected["camera"], true, "the target carries a camera: {connected}");

    // As the page does, the format goes the moment the socket opens: ahead of the
    // engine's RFB handshake, as the microphone's plug does.
    let mut camera = common::connect_camera_ws(addr, &token, &cookie).await;
    camera
        .send(Message::text(
            r#"{"type":"cameraFormat","width":320,"height":240,"fpsNumerator":15,"fpsDenominator":1}"#,
        ))
        .await
        .unwrap();
    let session = tokio::spawn(async move { while let Some(Ok(_)) = ws.next().await {} });

    let node = until_node(&container, "wlshare-camera-", true).await.expect("a node");
    // The application: GStreamer's PipeWire source, which ends after a count of
    // pictures and fails on a caps it cannot take. It prints the caps it negotiated.
    // Its stream names no media type of its own, and WirePlumber's linking scripts
    // fail on one without it and report the target as not found, so it is given the
    // properties a camera application's stream carries. The sink takes pictures as
    // they come rather than against the pipeline's clock: counting them is the point.
    let mut application = tokio::spawn(exec(
        &container,
        format!(
            "XDG_RUNTIME_DIR=/tmp/xdg timeout 60 gst-launch-1.0 -v pipewiresrc target-object={node} \
             stream-properties='props,media.type=Video,media.category=Capture,media.role=Camera' \
             num-buffers={CAMERA_BUFFERS} ! fakesink sync=false"
        ),
    ));
    // A start that never comes is an application that never opened the camera, which
    // the application's own task says better than a timeout does.
    let start = tokio::select! {
        start = uplink_signal(&mut camera) => start,
        ended = &mut application => panic!("the application ended before the camera started: {ended:?}"),
    };
    assert_eq!(start["type"], "cameraStart", "opening the camera starts the browser's: {start}");
    assert_eq!(
        (&start["width"], &start["height"], &start["fpsNumerator"], &start["fpsDenominator"]),
        (&320.into(), &240.into(), &15.into(), &1.into()),
        "the remote starts the format the browser plugged: {start}"
    );

    // Sent in real time while the remote streams, as the page sends: from a keyframe at
    // every start and whenever one is asked for, and nothing between a stop and the next
    // start. An application linking can stop and restart the stream as it settles its
    // format, so a stop is not the end; the application ending is.
    let units = access_units(CAMERA_H264);
    let next_keyframe = |from: usize| (from..).find(|n| units[n % units.len()].keyframe).expect("a keyframe");
    let mut tick = tokio::time::interval(Duration::from_millis(1000 / 15));
    let mut streaming = true;
    let mut next = 0;
    let mut sent = 0;
    let ended = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            tokio::select! {
                _ = tick.tick(), if streaming => {
                    let unit = &units[next % units.len()];
                    let mut frame = vec![remotex::protocol::camera::FRAME_KIND, u8::from(unit.keyframe)];
                    frame.extend_from_slice(&unit.data);
                    camera.send(Message::binary(frame)).await.unwrap();
                    next += 1;
                    sent += 1;
                }
                signal = uplink_signal(&mut camera) => match signal["type"].as_str() {
                    Some("cameraStart") => {
                        streaming = true;
                        next = next_keyframe(next);
                    }
                    Some("cameraStop") => streaming = false,
                    Some("cameraKeyframe") => next = next_keyframe(next),
                    _ => panic!("unexpected camera signal: {signal}"),
                },
                ended = &mut application => break ended,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the application never took {CAMERA_BUFFERS} pictures; {sent} were sent"));
    let negotiated = ended.expect("application task");

    // The application's leaving stops the stream.
    if streaming {
        tokio::time::timeout(Duration::from_secs(10), async {
            while uplink_signal(&mut camera).await["type"] != "cameraStop" {}
        })
        .await
        .expect("the application's leaving stops the browser's camera");
    }
    assert!(
        negotiated.contains("video/x-raw") && negotiated.contains("width=(int)320") && negotiated.contains("height=(int)240"),
        "the application takes pictures of the plugged size:\n{negotiated}"
    );

    camera.close(None).await.unwrap();
    until_node(&container, "wlshare-camera-", false).await;
    session.abort();
}
