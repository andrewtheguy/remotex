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

mod common;

use std::net::SocketAddr;
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
/// `resize`, and nothing naming the server — the extensions are discovered.
async fn spawn_app(vnc_port: u16) -> SocketAddr {
    let config = AppConfig {
        listen: remotex::config::ListenAddr::Tcp("127.0.0.1:0".to_owned()),
        static_dir: "frontend/dist".into(),
        auth: common::test_auth(),
        branding: remotex::config::Branding { text: "remotex".to_owned(), logo: None },
        dev_hostname: None,
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
            camera: false,
            microphone: false,
            render_type: remotex::config::RenderType::Tiles,
            render_subtype: None,
            render_stream_quality: None,
            render_subtype_quality: None,
            render_motion: false,
            render_motion_debug: false,
            render_chroma: None,
            render_classify_lossy: None,
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
    let app = server::router(config);
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

    let addr = spawn_app(vnc_port).await;
    let cookie = common::login(addr).await;
    let token = common::claim_session(addr, &cookie).await;
    let mut ws = common::connect_ws(addr, &token, &cookie).await;
    let mut view = View::new();

    // A 2x browser opens the session. wlshare reports HEADLESS-1 at scale 1, the
    // gateway declares 2x, and the output is set to it: the same pixels are
    // relabelled at 2x and repainted.
    ws.send(Message::text(format!(
        r#"{{"type":"connect","target":"{TARGET}","display":{{"w":1728,"h":1117,"scale":200}}}}"#
    )))
    .await
    .unwrap();
    let doubled = Size { w: 1024, h: 768, scale: 2.0 };
    view.until(&mut ws, "HEADLESS-1 relabelled at 2x", |v| {
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
    assert_eq!(sway_output(&container, "HEADLESS-1"), (1024, 768, 2.0));

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
