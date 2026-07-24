//! `remotex-agent` — the macOS screen-sharing agent remotex dials over `rxa`.
//!
//! remotex reaches a Mac today over Apple's Screen Sharing (VNC), which drops
//! and then demands a fresh login on every reconnect — the credential prompt
//! belongs to Apple's server, so there is nothing to fix on remotex's side of
//! the RFB connection. This agent replaces that hop: one pre-shared key, a
//! two-message Noise handshake, and no human in a reconnect ever. See
//! `docs/mac-agent-plan.md`.
//!
//! ## Installing is dragging it in and opening it
//!
//! There is no install script. On first launch the agent
//!
//! 1. writes `~/Library/Application Support/remotex-agent/config.toml` with a
//!    freshly generated pre-shared key, if it is not already there, and
//! 2. registers itself with `SMAppService` (see [`loginitem`]), which puts it in
//!    **System Settings → General → Login Items** and starts it at every login.
//!
//! Uninstalling is moving the bundle to the Trash — or `--unregister` first, to
//! take it out of Login Items cleanly.
//!
//! ## Two permissions
//!
//! - **Screen Recording**, for `SCStream` (see [`capture`]).
//! - **Accessibility**, for `CGEventPost` (see [`input`]).
//!
//! Both are one-time grants in System Settings → Privacy & Security, both are
//! attached to this bundle's signed identity, and macOS provides no way to grant
//! them programmatically. Accessibility is the one that bites: without it the
//! screen paints, the session looks perfectly healthy, and every click and
//! keystroke is silently discarded — so [`report_permissions`] says so at
//! startup.
//!
//! Both also require the process to live in the user's GUI (Aqua) session, which
//! is why the embedded plist is a LaunchAgent and not a LaunchDaemon. The honest
//! consequence: the agent is **not running at the login window** and cannot be.
//! If nobody is logged in on the Mac, there is nothing to connect to.
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
mod loginitem;
mod session;

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use log::{info, warn};

/// How often the main thread re-reads the system cursor.
const CURSOR_POLL: Duration = Duration::from_millis(100);

fn main() -> anyhow::Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    init_logging();

    // Subcommands that do one thing and exit, before any config is needed.
    if args.gen_psk {
        println!("{}", rxa_proto::psk::generate());
        return Ok(());
    }
    if args.unregister {
        loginitem::unregister()?;
        println!("remotex-agent removed from Login Items.");
        println!("Move remotex-agent.app to the Trash to finish uninstalling.");
        return Ok(());
    }

    let (config, path, created) = config::load_or_create(args.config.as_deref())?;
    if args.show_psk {
        println!("{}", config.psk);
        return Ok(());
    }
    if args.status {
        print_status(&config, &path);
        return Ok(());
    }

    info!(
        "remotex-agent {} — rxa/{}, config {}",
        env!("CARGO_PKG_VERSION"),
        rxa_proto::VERSION,
        path.display()
    );
    if created {
        info!("config: created {} with a fresh pre-shared key", path.display());
    }

    // Registering is idempotent, so doing it on every launch keeps a bundle that
    // was copied to a new machine (or a new user account) working without a
    // separate setup step. `--no-register` is for running it by hand in a
    // terminal while developing, where a login item would be in the way.
    if !args.no_register {
        match loginitem::register() {
            Ok(()) => info!("login item: registered ({})", loginitem::status()),
            // Not fatal: an agent started by hand should still serve. Most
            // likely causes are an unsigned bundle or no bundle at all.
            Err(e) => warn!("login item: could not register: {e:#}"),
        }
    }

    // When the user has just double-clicked the bundle, the terminal-less
    // launch has nowhere to show the key — so put it in the log, which
    // `--show-psk` also prints. The config file is already 0600.
    if created && std::io::stdout().is_terminal() {
        print_first_run(&config, &path);
    }

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
            match runtime.block_on(serve(listen, psk, display, serve_tracker)) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("remotex-agent: {e:#}");
                    std::process::exit(1);
                }
            }
        })?;

    // Main thread: poll the pointer shape forever. Sessions read the cache.
    loop {
        tracker.poll();
        std::thread::sleep(CURSOR_POLL);
    }
}

/// Log to stderr on a terminal, and to `~/Library/Logs/remotex-agent.log`
/// otherwise.
///
/// launchd does not expand `~`, so the embedded plist cannot name a per-user log
/// path and sets no `StandardErrorPath` at all — output would go nowhere. The
/// agent redirects its own logging instead, which also keeps a hand-run agent
/// printing to the terminal where you expect it.
fn init_logging() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if !std::io::stderr().is_terminal()
        && let Some(file) = log_file()
    {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
    builder.init();
}

fn log_file() -> Option<std::fs::File> {
    let home = std::env::var_os("HOME")?;
    let dir = Path::new(&home).join("Library/Logs");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("remotex-agent.log"))
        .ok()
}

/// Accept gateway connections, one at a time.
async fn serve(
    listen: String,
    psk: [u8; 32],
    display: usize,
    tracker: Arc<cursor::Tracker>,
) -> anyhow::Result<()> {
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(listener) => listener,
        // Almost always "another copy is already running" — launchd started one
        // at login and the user then opened the bundle by hand. That is not an
        // error worth a crash loop, so say so and exit cleanly.
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            info!("agent: {listen} is already in use — another remotex-agent is running; exiting");
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!("cannot bind {listen}: {e}")),
    };
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

/// Ask for both TCC grants at startup, and report where they stand.
///
/// Asking matters, and not only for politeness. Neither grant can be requested
/// implicitly by using the API:
///
/// - `SCShareableContent::get` does not prompt. It fails with "the user
///   declined TCCs", which reads like a refusal but also happens when the
///   question was never asked — and until it *is* asked, the agent does not
///   appear in the Screen Recording list at all, so there is nothing for the
///   user to switch on. `CGRequestScreenCaptureAccess` is what puts it there.
/// - `CGEventPost` never fails. Without Accessibility it silently does nothing,
///   so the screen paints, the session looks perfectly healthy, and every click
///   and keystroke vanishes.
///
/// macOS remembers the answer, so a granted (or firmly refused) permission does
/// not re-prompt on later launches.
fn report_permissions() {
    if capture::screen_recording_granted() {
        info!("permissions: Screen Recording granted");
    } else {
        warn!(
            "permissions: Screen Recording NOT granted — requesting it. Enable \
             remotex-agent in System Settings > Privacy & Security > Screen \
             Recording, then restart the agent."
        );
        capture::request_screen_recording();
    }

    if input::accessibility_granted() {
        info!("permissions: Accessibility granted (input injection will work)");
    } else {
        warn!(
            "permissions: Accessibility NOT granted — keyboard and mouse input \
             will be silently ignored. Grant it to remotex-agent in System \
             Settings > Privacy & Security > Accessibility."
        );
        input::request_accessibility();
    }
}

fn print_status(config: &config::Config, path: &Path) {
    println!("remotex-agent {}", env!("CARGO_PKG_VERSION"));
    println!("  config:        {}", path.display());
    println!("  listen:        {}", config.listen);
    println!("  display:       {}", config.display);
    println!("  login item:    {}", loginitem::status());
    println!(
        "  Screen Recording: {}",
        match capture::probe(config.display) {
            Ok(geometry) => format!(
                "granted ({}x{} at {}x)",
                geometry.width, geometry.height, geometry.scale
            ),
            Err(e) => format!("NOT granted or unavailable — {e}"),
        }
    );
    println!(
        "  Accessibility:    {}",
        if input::accessibility_granted() {
            "granted"
        } else {
            "NOT granted (input will be silently ignored)"
        }
    );
}

fn print_first_run(config: &config::Config, path: &Path) {
    println!();
    println!("Set up {}.", path.display());
    println!();
    println!("Put this on the gateway's rxa target:");
    println!();
    println!("    psk = \"{}\"", config.psk);
    println!();
    println!("Then grant two permissions in System Settings > Privacy & Security,");
    println!("enabling \"remotex-agent\" under BOTH of:");
    println!();
    println!("    Screen Recording   — without it the screen never paints");
    println!("    Accessibility      — without it input is silently ignored");
    println!();
}

/// The agent's argument surface, kept deliberately tiny — `clap` would be a
/// dependency for a handful of flags.
#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    gen_psk: bool,
    show_psk: bool,
    status: bool,
    no_register: bool,
    unregister: bool,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut parsed = Args::default();
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
                "--show-psk" => parsed.show_psk = true,
                "--status" => parsed.status = true,
                "--no-register" => parsed.no_register = true,
                "--unregister" => parsed.unregister = true,
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

With no options: create the config if absent, register as a login item, and
serve. Normally launched by macOS at login rather than by hand.

Options:
  -c, --config <path>  Config file (default:
                       ~/Library/Application Support/remotex-agent/config.toml)
      --show-psk       Print this agent's pre-shared key and exit
      --gen-psk        Print a fresh pre-shared key and exit
      --status         Show config, login-item and permission state, then exit
      --no-register    Serve without registering as a login item (for development)
      --unregister     Remove from Login Items and exit
  -h, --help           Show this help
  -V, --version        Show the version

The key from --show-psk goes on the matching [[targets]] entry in the gateway's
remotex.toml, as `psk`. It is the only credential either side uses.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<Args> {
        Args::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn no_arguments_serves_with_the_default_config_path() {
        let args = parse(&[]).unwrap();
        assert!(args.config.is_none());
        assert!(!args.gen_psk && !args.show_psk && !args.status);
        // Registering is the default: installing is meant to be "open it once".
        assert!(!args.no_register);
        assert!(!args.unregister);
    }

    #[test]
    fn config_is_accepted_short_and_long() {
        for flag in ["-c", "--config"] {
            let args = parse(&[flag, "/tmp/agent.toml"]).unwrap();
            assert_eq!(args.config, Some(PathBuf::from("/tmp/agent.toml")));
        }
    }

    #[test]
    fn every_flag_parses() {
        assert!(parse(&["--gen-psk"]).unwrap().gen_psk);
        assert!(parse(&["--show-psk"]).unwrap().show_psk);
        assert!(parse(&["--status"]).unwrap().status);
        assert!(parse(&["--no-register"]).unwrap().no_register);
        assert!(parse(&["--unregister"]).unwrap().unregister);
    }

    #[test]
    fn flags_combine_with_a_config_path() {
        let args = parse(&["--config", "/tmp/a.toml", "--status"]).unwrap();
        assert_eq!(args.config, Some(PathBuf::from("/tmp/a.toml")));
        assert!(args.status);
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

    // The usage text is the only documentation a user gets from the binary, so
    // it must mention every flag `parse` accepts.
    #[test]
    fn the_usage_text_documents_every_flag() {
        for flag in [
            "--config",
            "--show-psk",
            "--gen-psk",
            "--status",
            "--no-register",
            "--unregister",
            "--help",
            "--version",
        ] {
            assert!(USAGE.contains(flag), "usage does not mention {flag}");
        }
    }
}
