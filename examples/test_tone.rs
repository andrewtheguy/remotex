//! Serve the real gateway with a **generated tone** in place of a remote's
//! audio, so the browser half of the audio path can be listened to without a
//! server that redirects.
//!
//! It exists because the RDP side and the browser side fail independently, and
//! only one of them needs a Windows host. Whenever the representation changes —
//! and it has twice, from an open-ended WAV to Ogg/Opus to bare Opus packets
//! decoded by WebCodecs — the question is whether *browsers* play what this now
//! sends, live and without stalling. The PCM's provenance is irrelevant to that,
//! so this supplies it locally and the answer is unambiguous: a failure here is
//! the format, not the remote.
//!
//! The tone comes and goes in five-second phases, with the format published and
//! cleared around the gaps the way a real host's channel opening and closing
//! does. That is deliberate: the behaviour worth checking in a browser is no
//! longer only "does it play" but "does it *start on its own*, stop, and start
//! again" — so unmute during a quiet phase and then touch nothing.
//!
//! An example, run on demand, and never by a test run: it serves until it is
//! stopped. It is the gateway's own router in front of a scripted engine
//! ([`SessionManager::with_test_spawner`]), and it adds nothing a real deployment
//! could reach — no config key, no debug flag, no tone in the shipping product.
//!
//! ```sh
//! cargo run --example test_tone
//! ```
//!
//! Then open the printed URL, log in, pick the target, and press ☰ → Enable
//! audio. A 440 Hz tone means the whole browser-side path works, on that
//! browser — and **this is where a codec's browser support is settled**,
//! because there is no fallback: a browser whose `AudioDecoder` will not take
//! what the gateway sends says so under the button and plays nothing. Worth
//! running on each browser that matters rather than trusting a support table;
//! the last representation needed Safari 18.4 and plenty of published tables
//! still said it was unsupported.

use std::io::Write as _;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::mpsc::error::TryRecvError;

use remotex::audio::{AudioBridge, PCM_CD_QUALITY};
use remotex::config::{Protocol, TargetConfig};
use remotex::protocol::{ServerMsg, UNSCALED};
use remotex::session::SessionManager;

#[tokio::main]
async fn main() {
    /// One 20 ms buffer of 440 Hz stereo sine, from `phase` in samples.
    fn tone(phase: &mut u32) -> Vec<u8> {
        const HZ: f32 = 440.0;
        let frames = PCM_CD_QUALITY.sample_rate / 50;
        let mut buf = Vec::with_capacity(frames as usize * 4);
        for _ in 0..frames {
            let t = *phase as f32 / PCM_CD_QUALITY.sample_rate as f32;
            let sample = ((t * HZ * std::f32::consts::TAU).sin() * 8000.0) as i16;
            // Both channels, little-endian: the layout the queue carries and
            // the encoder deinterleaves.
            buf.extend_from_slice(&sample.to_le_bytes());
            buf.extend_from_slice(&sample.to_le_bytes());
            *phase = phase.wrapping_add(1);
        }
        buf
    }

    let target = TargetConfig {
        name: "test-tone".to_owned(),
        protocol: Protocol::Rdp,
        subtype: None,
        host: "127.0.0.1".to_owned(),
        port: 9, // discard: this engine is scripted, nothing is dialed
        username: String::new(),
        password: String::new(),
        vnc_password: String::new(),
        domain: None,
        size: Some((640, 480)),
        egfx: None,
        virtual_displays: 1,
        camera: false,
        microphone: false,
        video_quality: None,
        render_adaptive: None,
        virtual_display: false,
        audio_adaptive: None,
    };

    // The scripted engine: announce a desktop size so the SPA leaves its
    // "waiting for the remote desktop" overlay and shows the floating menu,
    // then feed the bridge in real time. A plain thread rather than a task
    // because everything it touches is synchronous, and it holds both channel
    // ends so the session layer sees a live engine. A session started without
    // sound is given no bridge, and is the same desktop with nothing to play.
    let sessions = Arc::new(SessionManager::with_test_spawner(
        vec![target.clone()],
        |_target, _choices, input_rx, frame_tx, audio: Option<Arc<AudioBridge>>, _camera| {
            std::thread::spawn(move || {
                let mut input_rx = input_rx;
                let size = ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED };
                if frame_tx.blocking_send(size.clone()).is_err() {
                    return;
                }
                // Paced against a deadline rather than by sleeping a fixed
                // 20 ms: the per-iteration overhead makes a fixed sleep
                // deliver ~2.5 s of audio every 3 s, and a browser would
                // stutter on the underrun — which is exactly the symptom this
                // harness exists to measure honestly.
                let buffer = std::time::Duration::from_millis(20);
                let mut phase = 0u32;
                let mut due = std::time::Instant::now();
                // 250 buffers of 20 ms: five seconds of tone, then five of the
                // remote being quiet, which on a real host means the audio
                // channel closing and negotiating again.
                let mut left_in_phase = 0u32;
                let mut playing = false;
                while !frame_tx.is_closed() {
                    if left_in_phase == 0 {
                        playing = !playing;
                        left_in_phase = 250;
                        match &audio {
                            Some(audio) if playing => audio.publish_format(PCM_CD_QUALITY),
                            Some(audio) => audio.clear_format(),
                            None => {}
                        }
                    }
                    left_in_phase -= 1;
                    if let Some(audio) = audio.as_ref().filter(|_| playing) {
                        audio.wave(tone(&mut phase));
                    }
                    // Answer `Refresh` by re-announcing the size, which is what
                    // every real engine does and what a reattaching browser
                    // depends on: the session layer injects it on every attach,
                    // and a client that never hears a size sits on "waiting for
                    // the remote desktop" forever. Polled on the audio cadence
                    // rather than blocked on, since this thread owes the queue a
                    // buffer every 20 ms. A session that dropped its input is gone.
                    loop {
                        match input_rx.try_recv() {
                            Ok(remotex::protocol::ClientMsg::Refresh) => {
                                if frame_tx.blocking_send(size.clone()).is_err() {
                                    return;
                                }
                            }
                            Ok(_) => {}
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }
                    due += buffer;
                    if let Some(nap) = due.checked_duration_since(std::time::Instant::now()) {
                        std::thread::sleep(nap);
                    }
                }
            });
        },
    ));

    let config = remotex::config::AppConfig {
        listen: remotex::config::ListenAddr::Tcp("127.0.0.1:0".to_owned()),
        targets: vec![target],
        auth: remotex::auth::GatewayAuth::Login(
            remotex::auth::SitePasswd::parse(
                &remotex::auth::generate("admin", "hunter2", 4).unwrap(),
            )
            .unwrap(),
        ),
        branding: remotex::config::Branding {
            text: "audio tone harness".to_owned(),
            logo: None,
        },
        dev_hostname: None,
        meter: None,
        hevc_wasm: None,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = remotex::server::router_with_sessions(config, sessions, remotex::throughput::Throughput::default(), None);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // println! rather than log: this is the harness's whole user interface.
    println!("\n  Open  http://{addr}/   (admin / hunter2)");
    println!("  Open \"test-tone\", choose Opus under Sound and Start. 440 Hz for 5s, quiet for 5s.");
    println!("  The tone must arrive on its own, go away, and come back, untouched,");
    println!("  and ☰ → Mute and Unmute must stop and start it.");
    println!("  Serving Opus through WebCodecs. A line under the button instead");
    println!("  means this browser has no decoder for it.");
    // A real RDP target started with sound separately covers server negotiation.
    println!("  Ctrl-C when done.\n");
    std::io::stdout().flush().unwrap();
    tokio::signal::ctrl_c().await.expect("waiting for Ctrl-C");
}
