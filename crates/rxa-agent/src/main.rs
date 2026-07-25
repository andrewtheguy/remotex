//! `remotex-agent` — the macOS screen-sharing agent remotex dials over `rxa`.
//!
//! remotex reaches a Mac today over Apple's Screen Sharing (VNC), which drops
//! and then demands a fresh login on every reconnect — the credential prompt
//! belongs to Apple's server, so there is nothing to fix on remotex's side of
//! the RFB connection. This agent replaces that hop: one pre-shared key, a
//! two-message Noise handshake, and no human in a reconnect ever. See
//! `docs/mac-agent-architecture.md`.
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
//! Uninstalling is switching **Start at Login** off in the menu and moving the
//! bundle to the Trash. Trashing it without that leaves a dangling Login Items
//! entry.
//!
//! ## Everything happens in the menu bar
//!
//! There are no windows, but there is a status item (see [`menubar`]), and it is
//! the entire interface: whether a gateway is connected, the pre-shared key and a
//! button to mint a new one, the listen address, which display is shared, the
//! config file, the log, the two Privacy panes, the login item, and Quit.
//! Anything the agent can be asked to do is done there.
//!
//! The flags below are launch modes only — where to read the config, and whether
//! to register or to put up a menu at all. No operation has a flag: a permission
//! read from a terminal is the *terminal's* permission (see
//! [`report_permissions`]), and a key printed to a terminal is a credential in
//! somebody's shell history.
//!
//! ## Two permissions
//!
//! - **Screen Recording**, for `SCStream` (see [`capture`]).
//! - **Accessibility**, for `CGEventPost` (see [`input`]).
//!
//! Both are one-time grants in System Settings → Privacy & Security, both are
//! attached to this bundle's signed identity, and macOS provides no way to grant
//! them programmatically. Neither is optional, so neither is a setting: the menu
//! bar treats them as health, warns in its icon and links to the missing one (see
//! [`menubar`]). Accessibility is the one that bites — without it the screen
//! paints, the session looks perfectly healthy, and every click and keystroke is
//! silently discarded.
//!
//! Both also require the capture process to live in a GUI session, which is why
//! the embedded plist is a LaunchAgent and not a LaunchDaemon. The current
//! per-user `SMAppService` registration runs only in the user's Aqua session. A
//! system-installed LaunchAgent could also target `LoginWindow`, but this app
//! does not install that privileged system-wide mode (see `docs/roadmap.md`).
//!
//! ## Threading
//!
//! AppKit is main-thread-only, and both the menu bar and reading the pointer
//! shape through `NSCursor` are AppKit — so the **main thread runs the
//! `NSApplication` run loop** (see [`menubar`]), polling the cursor into a cache
//! from a timer on it, while the tokio runtime serving the socket runs on a
//! thread of its own. Sessions only ever read that cache. ScreenCaptureKit needs
//! no run loop; it delivers frames on its own dispatch queues.
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
mod menubar;
mod panels;
mod pasteboard;
mod session;
mod settings;
mod state;

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use file_rotate::{
    ContentLimit, FileRotate, compression::Compression, suffix::AppendCount,
};
use log::{info, warn};

/// How often the main thread re-reads the system cursor when `--no-menu` has
/// taken the run loop away. The menu bar polls at the same rate from a timer.
const CURSOR_POLL: Duration = Duration::from_millis(100);
const LOG_FILE_BYTES: usize = 5 * 1024 * 1024;
const LOG_FILE_BACKUPS: usize = 3;

fn main() -> anyhow::Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    let log_path = init_logging();

    let (config, path, created) = match config::load_or_create(args.config.as_deref()) {
        Ok(loaded) => loaded,
        Err(e) => {
            // Exits 0 despite failing, so launchd's KeepAlive leaves it alone: no
            // number of restarts fixes a config file, and each one would put this
            // panel back on screen.
            report_startup_failure(
                &args,
                "remotex-agent could not start",
                &format!("{e:#}\n\nFix the config file, then open remotex-agent again."),
            );
            return Ok(());
        }
    };

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

    // Only worth printing where somebody can read it, and the key is not in it:
    // secrets stay out of log files, out of shell history and out of a terminal
    // somebody may be screen-sharing. Reading it is a menu item.
    if created && std::io::stdout().is_terminal() {
        print_first_run(&path);
    }

    // Bound on the main thread, and early — because "the port is taken" is the
    // one startup failure a user actually meets, and it has to be answerable on
    // screen. It happens whenever the app is opened while a copy is already
    // running, which is the normal way to go looking for the menu bar item.
    // Binding on the network thread instead left that thread calling `exit(0)`
    // from under a main thread that had not put up a menu yet: the app bounced
    // and vanished, and the only way to find out why was to run the binary in a
    // terminal.
    //
    // After the login-item registration above, though, and deliberately: opening
    // a freshly copied bundle while the old one still runs is how a stale launchd
    // record gets repaired (see packaging/macos/README.md), and that has to keep
    // working. Before `report_permissions`, equally deliberately: a duplicate
    // launch has no business raising a TCC prompt.
    // Infallible after the load above validated it, the same way `psk_bytes` is —
    // and `?` here would be one more way for a launch to end with nothing on
    // screen, which is the thing this whole block exists to stop.
    let addr = config
        .socket_addr()
        .expect("listen validated in Config::validate");
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            report_startup_failure(
                &args,
                "remotex-agent cannot listen",
                &format!(
                    "{} is already in use by another process.\n\nIf that process is another copy \
                     of remotex-agent, its icon is in the menu bar at the top of the screen.",
                    config.listen
                ),
            );
            return Ok(());
        }
        Err(e) => {
            report_startup_failure(
                &args,
                "remotex-agent cannot listen",
                &format!(
                    "{} could not be bound: {e}\n\nChange the listen address from the menu \
                     bar item of the running agent, or in the config file.",
                    config.listen
                ),
            );
            return Ok(());
        }
    };
    info!("agent: listening on {}", config.listen);
    // tokio adopts it below, and only a non-blocking socket can be driven by a
    // reactor.
    if let Err(e) = listener.set_nonblocking(true) {
        report_startup_failure(
            &args,
            "remotex-agent cannot listen",
            &format!("{} could not be made non-blocking: {e}", config.listen),
        );
        return Ok(());
    }

    // Keep the pre-request Screen Recording state: granting it in the system
    // prompt below changes TCC immediately, but capture only works after this
    // process is relaunched. The menu must not mistake that new TCC value
    // for a permission effective in this launch.
    let screen_recording_at_launch = report_permissions();

    let tracker = Arc::new(cursor::Tracker::new());
    let state = Arc::new(state::AgentState::new());
    // The GUI's view of the config: what this process is serving, and what the
    // file says after any edits made from the menu (see `crate::settings`).
    let settings = settings::Settings::new(config.clone(), path);

    // The socket runs on its own thread so the main thread stays free for
    // AppKit, which owns the menu bar and the pointer shape both.
    let serve_tracker = Arc::clone(&tracker);
    let serve_state = Arc::clone(&state);
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
            match runtime.block_on(serve(listener, psk, display, serve_tracker, serve_state)) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("remotex-agent: {e:#}");
                    std::process::exit(1);
                }
            }
        })?;

    if args.no_menu {
        // No run loop, so nothing drives an NSTimer: poll the pointer shape the
        // plain way instead. Sessions read the same cache either way.
        loop {
            tracker.poll();
            std::thread::sleep(CURSOR_POLL);
        }
    }

    // Hands the main thread to AppKit and never returns.
    menubar::run(
        state,
        tracker,
        settings,
        log_path,
        screen_recording_at_launch,
    )
}

/// Say why the agent is about to give up, on screen as well as in the log.
///
/// Everything this reports happens before the menu bar exists, which is what
/// makes it worth a function: the agent has no window, so a startup that fails
/// silently is a double-click that does nothing at all — no icon, no error, and
/// no way to find out short of running the binary in a terminal. That is not a
/// diagnosis anyone should have to make.
///
/// `--no-menu` gets the log line only. There is no window server to put a panel
/// in over SSH, and the caller is a terminal that can read the message.
fn report_startup_failure(args: &Args, title: &str, body: &str) {
    warn!("{title}: {body}");
    if args.no_menu {
        return;
    }
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        // Startup failures are all reported from `main`, so this is unreachable —
        // and a wrong-thread AppKit call is worse than a missing panel.
        return;
    };
    panels::startup_failure(mtm, title, body);
}

/// Restart the agent in place, to run under a config that has just changed.
///
/// `exec`, not "quit and be relaunched", and not "spawn a copy and exit":
///
/// - The embedded LaunchAgent's `KeepAlive` is `SuccessfulExit: false` — that is
///   what makes Quit mean Quit (see [`menubar`]) — so a clean exit would leave the
///   agent stopped, not restarted.
/// - Spawning a second copy first loses a race with its own listener: the new
///   process binds before this one has let the port go, finds it in use, and
///   exits cleanly (see [`serve`]), leaving nothing running.
///
/// Replacing the process image has neither problem. The PID, the launchd job and
/// the code identity the two TCC grants are keyed to all survive, the listening
/// socket closes on the way (Rust opens sockets `CLOEXEC`), and the new image
/// re-reads the config from scratch. The gateway sees a dropped connection and
/// reconnects, which it is already built to do.
///
/// Only returns if the exec failed, in which case nothing has changed and the
/// caller still has a running agent to report to.
fn restart() -> anyhow::Error {
    use std::os::unix::process::CommandExt as _;

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return anyhow::anyhow!("cannot find my own executable: {e}"),
    };
    info!("restarting into {}", exe.display());
    // Same arguments, so a `--config` path or a `--no-menu` session restarts as
    // itself rather than as a default agent.
    let error = std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .exec();
    anyhow::anyhow!("cannot restart {}: {error}", exe.display())
}

/// Log to stderr on a terminal, and to a bounded set of files rooted at
/// `~/Library/Logs/remotex-agent.log` otherwise.
///
/// launchd does not expand `~`, so the embedded plist cannot name a per-user log
/// path and sets no `StandardErrorPath` at all — output would go nowhere. The
/// agent redirects its own logging instead, which also keeps a hand-run agent
/// printing to the terminal where you expect it.
///
/// Returns the file it chose, if any, so the menu bar can offer to open it —
/// and, on a terminal, correctly offers nothing.
fn init_logging() -> Option<PathBuf> {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    let mut path = None;
    if !std::io::stderr().is_terminal()
        && let Some((file, chosen)) = log_file()
    {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
        path = Some(chosen);
    }
    builder.init();
    path
}

fn log_file() -> Option<(FileRotate<AppendCount>, PathBuf)> {
    let home = std::env::var_os("HOME")?;
    let dir = Path::new(&home).join("Library/Logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("remotex-agent.log");
    let writer = FileRotate::new(
        path.clone(),
        AppendCount::new(LOG_FILE_BACKUPS),
        ContentLimit::BytesSurpassed(LOG_FILE_BYTES),
        Compression::None,
        None,
    );
    Some((writer, path))
}

/// Accept gateway connections, one at a time.
///
/// Takes an already-bound listener: binding is the main thread's job, so a port
/// that is already taken can be reported on screen rather than from this thread
/// (see `main`).
async fn serve(
    listener: std::net::TcpListener,
    psk: [u8; 32],
    display: usize,
    tracker: Arc<cursor::Tracker>,
    state: Arc<state::AgentState>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| anyhow::anyhow!("cannot drive the listening socket: {e}"))?;

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
        // Recorded before the eviction, so the menu bar never blinks through a
        // "not connected" state during a reconnect. The id is what keeps the
        // evicted session from clearing this one on its way out.
        let id = state.connected(peer, Instant::now());
        if let Some(previous) = current.take() {
            info!("agent: evicting the previous gateway connection");
            previous.abort();
        }

        let tracker = Arc::clone(&tracker);
        let session_state = Arc::clone(&state);
        current = Some(tokio::spawn(async move {
            match session::serve(stream, psk, display, tracker).await {
                Ok(()) => info!("agent: gateway {peer} disconnected"),
                // Includes a wrong PSK, which is a failed handshake — logged and
                // dropped, never fatal to the agent.
                Err(e) => warn!("agent: session with {peer} ended: {e:#}"),
            }
            session_state.disconnected(id);
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
/// not re-prompt on later launches. The menu bar keeps missing grants visible and
/// links to their System Settings panes without showing a second dialog.
///
/// Returns whether Screen Recording was already granted before requesting it.
/// A grant made by the request belongs to a newly launched process.
fn report_permissions() -> bool {
    let screen_recording_at_launch = capture::screen_recording_granted();
    if screen_recording_at_launch {
        info!("permissions: Screen Recording granted");
    } else {
        warn!(
            "permissions: Screen Recording NOT granted — requesting it. Enable \
             remotex-agent in System Settings > Privacy & Security > Screen \
             Recording, then quit and reopen the agent."
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

    screen_recording_at_launch
}

fn print_first_run(path: &Path) {
    println!();
    println!("Set up {}, with a fresh pre-shared key.", path.display());
    println!();
    println!("The rest is in the menu bar, under the remotex-agent icon:");
    println!();
    println!("    Pre-Shared Key     — the one credential; it goes on the gateway target");
    println!("    Screen Recording   — without it the screen never paints");
    println!("    Accessibility      — without it input is silently ignored");
    println!();
}

/// The agent's argument surface: three launch modes and nothing else.
///
/// Every *operation* is a menu item (see [`menubar`]), so what is left here is
/// only how to start — which config to read, and whether to register and put up a
/// menu. `clap` would be a dependency for that.
#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    no_register: bool,
    no_menu: bool,
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
                "--no-register" => parsed.no_register = true,
                "--no-menu" => parsed.no_menu = true,
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
      --no-register    Serve without registering as a login item (for development)
      --no-menu        Serve without a menu bar item. Needed over SSH, where
                       there is no window server to put one in — and with no menu
                       there is no interface at all, so this is for development
  -h, --help           Show this help
  -V, --version        Show the version

Everything else is in the menu bar: the pre-shared key (which goes on the
matching [[targets]] entry in the gateway's remotex.toml, and is the only
credential either side uses), the listen address, the display, the config file,
the log, the two permissions, Start at Login, and Quit.
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
        // Registering is the default: installing is meant to be "open it once".
        assert!(!args.no_register);
        // So is the menu bar, which is the agent's whole interface — an agent
        // with no visible sign of itself, no way to read its key and no way to
        // quit is what --no-menu opts *out* of.
        assert!(!args.no_menu);
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
        assert!(parse(&["--no-register"]).unwrap().no_register);
        assert!(parse(&["--no-menu"]).unwrap().no_menu);
    }

    #[test]
    fn flags_combine_with_a_config_path() {
        let args = parse(&["--config", "/tmp/a.toml", "--no-menu"]).unwrap();
        assert_eq!(args.config, Some(PathBuf::from("/tmp/a.toml")));
        assert!(args.no_menu);
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
        for flag in ["--config", "--no-register", "--no-menu", "--help", "--version"] {
            assert!(USAGE.contains(flag), "usage does not mention {flag}");
        }
    }
}
