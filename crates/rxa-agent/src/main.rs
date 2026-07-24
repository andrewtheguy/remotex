//! `remotex-agent` — the macOS screen-sharing agent remotex dials over `rxa`.
//!
//! remotex reaches a Mac today over Apple's Screen Sharing (VNC), which drops
//! and then demands a fresh login on every reconnect — the credential prompt
//! belongs to Apple's server, so there is nothing to fix on remotex's side of
//! the RFB connection. This agent replaces that hop: one pre-shared key, a
//! two-message Noise handshake, and no human in a reconnect ever. See
//! `docs/mac-agent-plan.md`.
//!
//! ## Two permissions, and why it is a LaunchAgent
//!
//! - **Screen Recording**, for `SCStream` (see [`capture`]).
//! - **Accessibility**, for `CGEventPost` (see [`input`]).
//!
//! Both are one-time grants in System Settings → Privacy & Security, and both
//! require the process to live in the user's **GUI (Aqua) session**. That makes
//! this a *LaunchAgent*, not a LaunchDaemon: a daemon has no window server
//! connection and both capture and injection fail outright.
//!
//! The honest consequence: the agent is **not running at the login window** and
//! cannot be. If nobody is logged in on the Mac, there is nothing to connect to.
//!
//! ## Threading
//!
//! AppKit is main-thread-only, and reading the pointer shape goes through
//! `NSCursor` — so the **main thread polls the cursor** into a cache and the
//! tokio runtime serving the socket runs on a thread of its own. Sessions only
//! ever read that cache. ScreenCaptureKit needs no run loop; it delivers frames
//! on its own dispatch queues.
//!
//! ## One gateway at a time
//!
//! A new connection evicts the previous one, matching remotex's single-session
//! model (see CLAUDE.md): multi-session is permanently out of scope, and a
//! browser that force-claims the gateway's session slot should not find itself
//! queued behind a stale agent connection.

// A bare `#![cfg(target_os = "macos")]` would compile the crate away to
// nothing on Linux and fail at link time with "main function not found",
// which says nothing useful. Fail at compile time with the reason instead.
#[cfg(not(target_os = "macos"))]
compile_error!(
    "rxa-agent is macOS-only (ScreenCaptureKit + CoreGraphics). It is excluded \
     from the workspace's default-members; build it on a Mac with \
     `cargo build -p rxa-agent`."
);

mod capture;
mod config;
mod cursor;
mod encode;
mod input;
mod session;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use log::{info, warn};

/// How often the main thread re-reads the system cursor.
const CURSOR_POLL: Duration = Duration::from_millis(100);

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse(std::env::args().skip(1))?;
    if args.gen_psk {
        println!("{}", rxa_proto::psk::generate());
        return Ok(());
    }

    let (config, path) = config::load(args.config.as_deref())?;
    info!(
        "remotex-agent {} — rxa/{}, config {}",
        env!("CARGO_PKG_VERSION"),
        rxa_proto::VERSION,
        path.display()
    );

    report_permissions();

    let tracker = Arc::new(cursor::Tracker::new());

    // The socket runs on its own thread so the main thread stays free for
    // AppKit. `main` then becomes the cursor poller and never returns.
    let serve_tracker = Arc::clone(&tracker);
    let listen = config.listen.clone();
    let psk = config.psk_bytes();
    let display = config.display;
    std::thread::Builder::new()
        .name("rxa-net".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    // Without a runtime there is no agent; exiting lets
                    // launchd's KeepAlive restart us.
                    eprintln!("remotex-agent: cannot build the tokio runtime: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = runtime.block_on(serve(listen, psk, display, serve_tracker)) {
                eprintln!("remotex-agent: {e:#}");
                std::process::exit(1);
            }
        })?;

    // Main thread: poll the pointer shape forever. Sessions read the cache.
    loop {
        tracker.poll();
        std::thread::sleep(CURSOR_POLL);
    }
}

/// Accept gateway connections, one at a time.
async fn serve(
    listen: String,
    psk: [u8; 32],
    display: usize,
    tracker: Arc<cursor::Tracker>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {listen}: {e}"))?;
    info!("agent: listening on {listen}");

    // The single active session, so a new gateway evicts the previous one.
    let mut current: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // A per-connection accept error (e.g. EMFILE) must not kill the
                // agent; the next accept usually succeeds.
                warn!("agent: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        info!("agent: gateway connected from {peer}");
        if let Some(previous) = current.take() {
            info!("agent: evicting the previous gateway connection");
            previous.abort();
        }

        let tracker = Arc::clone(&tracker);
        current = Some(tokio::spawn(async move {
            match session::serve(stream, psk, display, tracker).await {
                Ok(()) => info!("agent: gateway {peer} disconnected"),
                // Includes a wrong PSK, which is a failed handshake — logged and
                // dropped, never fatal to the agent.
                Err(e) => warn!("agent: session with {peer} ended: {e:#}"),
            }
        }));
    }
}

/// Log the state of both TCC grants at startup.
///
/// Screen Recording is reported by the capture probe when a session starts;
/// Accessibility has no such natural moment — a missing grant makes
/// `CGEventPost` silently do nothing, so the screen paints, the session looks
/// healthy, and every click vanishes. Saying so here is the difference between a
/// two-click fix and an afternoon.
fn report_permissions() {
    if input::accessibility_granted() {
        info!("permissions: Accessibility granted (input injection will work)");
    } else {
        warn!(
            "permissions: Accessibility NOT granted — keyboard and mouse input \
             will be silently ignored. Grant it to remotex-agent in System \
             Settings > Privacy & Security > Accessibility."
        );
    }
}

/// The agent's argument surface, kept deliberately tiny — `clap` would be a
/// dependency for two flags.
#[derive(Debug)]
struct Args {
    config: Option<PathBuf>,
    gen_psk: bool,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut parsed = Args {
            config: None,
            gen_psk: false,
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-c" | "--config" => {
                    let path = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--config needs a path"))?;
                    parsed.config = Some(PathBuf::from(path));
                }
                "--gen-psk" => parsed.gen_psk = true,
                "-h" | "--help" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("remotex-agent {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                other => anyhow::bail!("unexpected argument {other:?}\n\n{USAGE}"),
            }
        }
        Ok(parsed)
    }
}

const USAGE: &str = "\
remotex-agent — the macOS screen agent remotex connects to

Usage: remotex-agent [options]

Options:
  -c, --config <path>  Config file (default:
                       ~/Library/Application Support/remotex-agent/config.toml)
      --gen-psk        Print a fresh pre-shared key and exit
  -h, --help           Show this help
  -V, --version        Show the version

The key printed by --gen-psk goes in two places: `psk` in this agent's config
and `psk` on the matching [[targets]] entry in the gateway's remotex.toml.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<Args> {
        Args::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn no_arguments_uses_the_default_config_path() {
        let args = parse(&[]).unwrap();
        assert!(args.config.is_none());
        assert!(!args.gen_psk);
    }

    #[test]
    fn config_is_accepted_short_and_long() {
        for flag in ["-c", "--config"] {
            let args = parse(&[flag, "/tmp/agent.toml"]).unwrap();
            assert_eq!(args.config, Some(PathBuf::from("/tmp/agent.toml")));
        }
    }

    #[test]
    fn gen_psk_is_a_flag() {
        assert!(parse(&["--gen-psk"]).unwrap().gen_psk);
    }

    #[test]
    fn a_config_flag_without_a_path_is_an_error() {
        let err = parse(&["--config"]).unwrap_err();
        assert!(format!("{err:#}").contains("needs a path"), "{err:#}");
    }

    #[test]
    fn unknown_arguments_are_rejected_with_the_usage() {
        let err = parse(&["--recursive"]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--recursive"), "{msg}");
        assert!(msg.contains("Usage:"), "{msg}");
    }
}
