//! `remotex-agent` — the macOS screen-sharing agent remotex dials over `rxa`.
//!
//! remotex reaches a Mac today over Apple's Screen Sharing (VNC), which drops
//! and then demands a fresh login on every reconnect — the credential prompt
//! belongs to Apple's server, so there is nothing to fix on remotex's side of
//! the RFB connection. This agent replaces that hop: a keypair on each end, a
//! two-message Noise handshake, and no human in a reconnect ever. See
//! `docs/mac-agent-architecture.md`.
//!
//! ## Installing is dragging it in and opening it
//!
//! There is no install script. On first launch the agent
//!
//! 1. writes `~/Library/Application Support/remotex-agent/config.toml` with a
//!    freshly minted keypair, if it is not already there, and
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
//! the entire interface: whether a gateway is connected, this Mac's public key
//! and the gateway's, the listen address, which display is shared, the config
//! file, the log, the two Privacy panes, the login item, and Quit. Anything the
//! agent can be asked to do is done there.
//!
//! The flags below are launch modes, plus the two that read and write this Mac's
//! identity. No *operation* has a flag: a permission read from a terminal is the
//! *terminal's* permission (see [`report_permissions`]).
//!
//! The identity flags are not exceptions to that so much as the thing the rule
//! was about, which is a *secret* in somebody's shell history. `--public-key`
//! prints the half that is not a secret. `--import-private-key` takes one that
//! is — from stdin, never an argument, so it reaches neither the history nor
//! `ps`. The private key is still never printed, by anything.
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
//! A newly *authenticated* connection evicts the previous one, matching
//! remotex's single-session model (see CLAUDE.md): multi-session is permanently
//! out of scope, and a browser that force-claims the gateway's session slot
//! should not find itself queued behind a stale agent connection.
//!
//! Authenticated is the load-bearing word — see [`serve`]. Anything that cannot
//! complete the handshake is refused without the running session noticing.

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
mod virtualdisplay;

use std::io::IsTerminal as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use file_rotate::{
    ContentLimit, FileRotate, compression::Compression, suffix::AppendCount,
};
use log::{info, warn};
use tokio::time::timeout;

/// How often the main thread re-reads the system cursor when `--no-menu` has
/// taken the run loop away. The menu bar polls at the same rate from a timer.
const CURSOR_POLL: Duration = Duration::from_millis(100);
const LOG_FILE_BYTES: usize = 5 * 1024 * 1024;
const LOG_FILE_BACKUPS: usize = 3;

fn main() -> anyhow::Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    let log_path = init_logging();

    let loaded = config::load_or_create(args.config.as_deref());

    // Answered before the login item, the bind and the TCC prompts: this is a
    // question about the config file, not a launch, and asking it must not
    // register anything, take the port from a running agent, or raise a prompt.
    //
    // `load_or_create`, so the first thing anyone does over SSH on a fresh Mac
    // mints the identity and prints it in one go. And `?` rather than the
    // report-and-exit-0 below, which exists so launchd's KeepAlive does not loop
    // on a broken config — for a question asked from a shell that would answer
    // with silence.
    if args.public_key {
        let (config, _, _) = loaded?;
        println!("{}", config.public_key());
        return Ok(());
    }

    // Same reasoning, and the same place in the launch: a config edit, not a
    // launch. Drops `loaded` because the import re-reads and rewrites the file
    // itself — and because on a Mac with no config yet, the identity that load
    // just minted is the one being replaced.
    if args.import_private_key {
        drop(loaded);
        let key = read_private_key_from_stdin()?;
        let config = config::import_private_key(args.config.as_deref(), &key)?;
        // The public half, because that is the value to check against whatever
        // the gateway already has — the point of importing rather than minting.
        println!("{}", config.public_key());
        return Ok(());
    }

    let (config, path, created) = match loaded {
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
        info!(
            "config: created {} with a fresh identity, unpaired",
            path.display()
        );
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

    // Only worth printing where somebody can read it. The public key *is* in it
    // now — it is the next thing to do, and it is not a secret; the private key
    // stays out of log files, out of shell history and out of a terminal
    // somebody may be screen-sharing.
    if created && std::io::stdout().is_terminal() {
        print_first_run(&path, &config.public_key());
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
    // Before the bind, not after losing it: whoever is not the LaunchAgent job
    // defers to the job, whether or not it would have won the port. The two
    // `--no-` flags are hand-run developer copies, which have no business
    // restarting the installed agent.
    if !args.no_menu && !args.no_register && hand_over_to_launchd() {
        return Ok(());
    }

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
    let keys = Keys {
        private: config.private_key_bytes(),
        gateway_public: config.gateway_public_key_bytes(),
    };
    if keys.gateway_public.is_none() {
        warn!(
            "no gateway_public_key: this Mac is unpaired and will refuse every \
             connection. Its public key is {} — paste that into the gateway's \
             agent_public_key, and the gateway's own into Settings.",
            config.public_key()
        );
    }

    // A display of our own, if the config asks for one. Created here, on the
    // main thread and before any session exists, because a display that came and
    // went with each connection would rearrange the windows on it every time
    // (see `crate::virtualdisplay`). Held for the life of the process: dropping
    // it is what removes the display.
    //
    // A failure costs the extra display and nothing else: the Mac's own screens
    // are still there to share, and a client that wanted the private one will
    // simply not find it in the list.
    let virtual_display = config
        .virtual_display
        .then(|| match config.virtual_display_initial_points() {
            Ok(points) => virtualdisplay::VirtualDisplay::create(points)
                .inspect_err(|e| warn!("virtualdisplay: {e:#}; the Mac's screens are unaffected"))
                .ok(),
            Err(e) => {
                warn!("virtualdisplay: {e:#}; the Mac's screens are unaffected");
                None
            }
        })
        .flatten();
    // Creating the display does not select it. It is an *additional* screen —
    // that is the whole of what `CGVirtualDisplay` does — so it joins the list a
    // client picks from rather than replacing what the Mac already has. Every
    // session starts on the main display; the choice after that is the viewer's.
    let owned = virtual_display
        .as_ref()
        .map(|display| capture::Target::Owned {
            id: display.id(),
            base_points: display.base_points(),
        });
    // Shared with the session so a client can set its density, and *only* its
    // density (`rxa_proto::msg::GatewayMsg::HostScale`). Behind a mutex rather
    // than an `unsafe impl Sync`: `applySettings:` is a WindowServer round trip
    // of 66-397 ms, and two of them racing is a question nothing here needs to
    // answer. The `Arc` also keeps the display alive for the life of the
    // process, which is what dropping it would end.
    let owned = session::Owned {
        target: owned,
        handle: virtual_display.map(|display| Arc::new(std::sync::Mutex::new(display))),
    };
    let serve_owned = owned.clone();

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
            match runtime.block_on(serve(
                listener,
                keys,
                serve_owned,
                serve_tracker,
                serve_state,
            )) {
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

/// Restart the agent to run under a config that has just changed, by asking
/// launchd to restart its job.
///
/// launchd does it, rather than this process doing it to itself. `exec` was the
/// obvious way and is the wrong one for an app with a menu bar item: the process
/// keeps the window server connection, the application identity and the Mach
/// ports the *old* image registered, and the new image's AppKit comes up on top of
/// them only half working. Measured on the test VM, with the status item forced
/// visible (see [`menubar::run`]): the icon drew as an empty pill whose menu would
/// not open, so the agent had saved the setting and could no longer be quit at
/// all. The same build restarted by launchd comes up clean every time.
///
/// Not "spawn a copy and exit" either, which loses a race with its own listener:
/// the new process binds before this one has let the port go, finds it in use, and
/// exits cleanly (see [`serve`]), leaving nothing running. launchd has no such
/// race — it takes this instance down and then starts the next one.
///
/// The PID changes now, which nothing depended on; the launchd job and the code
/// identity the two TCC grants are keyed to are exactly what is preserved. The
/// gateway sees a dropped connection and reconnects, which it is already built to
/// do.
///
/// Only returns if the restart could not be asked for at all — an agent that is
/// not registered as a login item has no job to kick — in which case nothing has
/// changed but the config file, and the caller says so on screen.
fn restart() -> anyhow::Error {
    let service = format!("gui/{}/{}", uid(), loginitem::LABEL);
    info!("restarting through launchd: {service}");
    match std::process::Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &service])
        .status()
    {
        // launchd takes this instance down as part of the restart, so the ordinary
        // end of this call is not returning from it.
        Ok(status) if status.success() => {
            std::thread::sleep(Duration::from_secs(5));
            anyhow::anyhow!("launchd accepted the restart of {service}, but this process is still running")
        }
        Ok(status) => anyhow::anyhow!(
            "launchctl could not restart {service} ({status}) — the agent may not be registered \
             as a login item"
        ),
        Err(e) => anyhow::anyhow!("cannot run launchctl to restart {service}: {e}"),
    }
}

/// Unless this process *is* the LaunchAgent job, start that job from this bundle
/// and let the caller exit. Returns whether it did.
///
/// One instance of this agent is the right number, and the LaunchAgent job is
/// which one: it is the copy that survives a logout, that the menu's Quit and a
/// settings save both restart, and that TCC's grants were issued to. So an
/// `open` is not a second agent — it is a request that the job be running, from
/// the bundle that was just opened.
///
/// The collision this ends is `open` on a bundle whose job is **registered but
/// not running** — a fresh install, and every upgrade done the documented way,
/// which quits the agent first. The opened copy registers the login item, that
/// registration bootstraps the job, launchd starts it, and now two processes are
/// starting at once with one port between them. Whichever lost used to park a
/// modal alert nobody was going to click; on the test VM that left a zombie
/// process behind every single deploy. Deferring *before* the bind, rather than
/// reacting after losing it, means there is no race to lose.
///
/// The other half of the same problem is not a collision and cannot be fixed
/// here: opening a new bundle while the old agent still runs launches nothing at
/// all, because macOS activates the running app instead — so the upgrade quietly
/// does not happen. That one is answered in the README, by quitting first.
///
/// It does mean opening the app restarts a running agent, dropping a session in
/// progress. That is the trade a settings save already makes and the gateway
/// already reconnects from; the alternative is an upgrade that cannot take effect
/// without a logout.
///
/// A job that is loaded but not yet running still counts — on a fresh install
/// that is exactly the state a few milliseconds after registering, and leaving it
/// to start on its own is how the collision happened. What does not count is a
/// pid that is ours: then we are the job, and kickstarting would be this process
/// restarting itself forever. Nor does a `print` that fails, which means no job —
/// an unregistered or hand-built copy, which should simply serve.
fn hand_over_to_launchd() -> bool {
    let service = format!("gui/{}/{}", uid(), loginitem::LABEL);
    let Ok(printed) = std::process::Command::new("/bin/launchctl")
        .args(["print", &service])
        .output()
    else {
        return false;
    };
    if !printed.status.success() {
        return false;
    }
    let job_pid = String::from_utf8_lossy(&printed.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = ")?.trim().parse::<u32>().ok());
    if job_pid == Some(std::process::id()) {
        return false;
    }
    info!("agent: {service} is this Mac's copy; starting it from this bundle and standing down");
    // `-k` so a job that is already running is restarted into this bundle, which
    // is the upgrade. Harmless on one that is merely loaded.
    matches!(
        std::process::Command::new("/bin/launchctl")
            .args(["kickstart", "-k", &service])
            .status(),
        Ok(status) if status.success()
    )
}

/// This user's uid, for the launchd domain the login item lives in.
fn uid() -> u32 {
    // No safe binding in std, and it cannot fail.
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
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

/// How long a peer has to complete a handshake before it is dropped.
///
/// Generous — this is two messages over a LAN — and its job is only to stop
/// half-open connections accumulating. The gateway gives the whole
/// connect-and-hello 10 seconds, so nothing legitimate is near this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Accept gateway connections, one at a time.
///
/// Takes an already-bound listener: binding is the main thread's job, so a port
/// that is already taken can be reported on screen rather than from this thread
/// (see `main`).
///
/// ## Only an authenticated peer takes the slot
///
/// There is one session slot and a new gateway evicts whoever is in it — but the
/// eviction happens when a connection *finishes its handshake*, not when it is
/// accepted. Evicting at accept meant anything that could reach this port could
/// end a live session by opening a socket, without holding a key or saying a
/// word: a port scanner, a stale gateway, a mistyped host in someone else's
/// config. Now a peer that cannot prove it is the paired gateway is refused
/// without the running session ever noticing.
///
/// Handshakes therefore run in their own tasks and report back over a channel,
/// rather than inline: one peer that connects and stays silent must not hold up
/// the accept loop, which is why [`HANDSHAKE_TIMEOUT`] exists too.
/// This Mac's identity and the one gateway it answers.
///
/// `gateway_public` is `None` while the agent is unpaired — a first launch, or a
/// config whose `gateway_public_key` was cleared. Then it still listens, so the
/// port is visibly answering and the menu bar is there to be paired from, but no
/// connection gets as far as a handshake.
#[derive(Clone, Copy)]
struct Keys {
    private: [u8; 32],
    gateway_public: Option<[u8; 32]>,
}

async fn serve(
    listener: std::net::TcpListener,
    keys: Keys,
    owned: session::Owned,
    tracker: Arc<cursor::Tracker>,
    state: Arc<state::AgentState>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| anyhow::anyhow!("cannot drive the listening socket: {e}"))?;

    // The single active session, so a new gateway evicts the previous one.
    let mut current: Option<tokio::task::JoinHandle<()>> = None;
    // Connections that have finished a handshake, waiting to take the slot.
    // Bounded, and generously: it holds only authenticated peers, and the loop
    // below drains it immediately.
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel::<(SocketAddr, session::Authenticated)>(4);

    loop {
        tokio::select! {
            // A connection that proved itself. *Now* the slot moves — see the
            // module docs on why this is not done at accept.
            Some((peer, authenticated)) = ready_rx.recv() => {
                info!("agent: gateway connected from {peer}");
                // Recorded before the eviction, so the menu bar never blinks
                // through a "not connected" state during a reconnect. The id is
                // what keeps the evicted session from clearing this one on its
                // way out.
                let id = state.connected(peer, Instant::now());
                if let Some(previous) = current.take() {
                    info!("agent: evicting the previous gateway connection");
                    previous.abort();
                }

                let tracker = Arc::clone(&tracker);
                let session_owned = owned.clone();
                let session_state = Arc::clone(&state);
                current = Some(tokio::spawn(async move {
                    match session::serve(authenticated, session_owned, tracker).await {
                        Ok(()) => info!("agent: gateway {peer} disconnected"),
                        Err(e) => warn!("agent: session with {peer} ended: {e:#}"),
                    }
                    session_state.disconnected(id);
                }));
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        // A per-connection accept error (e.g. EMFILE) must not
                        // kill the agent; the next accept usually succeeds.
                        warn!("agent: accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                // An unpaired agent has no key to judge anyone by, so this is as
                // far as any connection gets.
                let Some(gateway_public) = keys.gateway_public else {
                    warn!("agent: refusing {peer} — no gateway_public_key is set (open Settings)");
                    drop(stream);
                    continue;
                };

                // Off the accept path, and on a clock. Handshaking inline would
                // let one peer that connects and then says nothing block every
                // later connection — including the real gateway's — for as long
                // as it cared to hold the socket open.
                let ready_tx = ready_tx.clone();
                tokio::spawn(async move {
                    let handshake =
                        session::handshake(stream, keys.private, gateway_public);
                    match timeout(HANDSHAKE_TIMEOUT, handshake).await {
                        Ok(Ok(authenticated)) => {
                            // A full queue means four authenticated peers are
                            // already waiting, which one gateway at a time
                            // cannot produce. Dropping this one is right.
                            if ready_tx.try_send((peer, authenticated)).is_err() {
                                warn!("agent: dropping {peer}, too many pending sessions");
                            }
                        }
                        // Never fatal to the agent, and never visible to the
                        // session already running: an unpaired peer, a port
                        // scanner, or a gateway on another protocol version.
                        Ok(Err(e)) => warn!("agent: refusing {peer}: {e:#}"),
                        Err(_) => warn!(
                            "agent: refusing {peer}: no handshake within {}s",
                            HANDSHAKE_TIMEOUT.as_secs()
                        ),
                    }
                });
            }
        }
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

/// Read the key `--import-private-key` imports.
///
/// From stdin and nowhere else. As an argument it would be in the shell's
/// history and visible in `ps` to every user on the machine; this way
/// `pbpaste | … --import-private-key` or a redirect from a file leaves neither
/// trace.
///
/// The whole of stdin, not a line: a key pasted into a terminal, piped from
/// `pbpaste`, or read from a file that ends without a newline all have to work,
/// and none of them is distinguishable here.
///
/// Returns the key alone. Every one of those ways of supplying it brings its own
/// whitespace — a pipe from `pbpaste` most often a trailing newline — and
/// `config::import_private_key` trims again before it uses one, so this is not
/// what keeps a newline out of the config file. It is so that what this function
/// returns is a key, which is what its name promises.
fn read_private_key_from_stdin() -> anyhow::Result<String> {
    use anyhow::Context as _;
    use std::io::Read as _;

    // Said before the read, or a terminal with nothing piped into it looks like
    // a hang rather than a prompt.
    if std::io::stdin().is_terminal() {
        eprintln!("Paste this Mac's private key (rxas…), then press Ctrl-D:");
    }
    let mut key = String::new();
    std::io::stdin()
        .read_to_string(&mut key)
        .context("failed to read the private key from stdin")?;
    let key = key.trim();
    anyhow::ensure!(
        !key.is_empty(),
        "no private key on stdin — pipe one in, e.g. \
         `pbpaste | remotex-agent --import-private-key`"
    );
    Ok(key.to_owned())
}

fn print_first_run(path: &Path, public_key: &str) {
    println!();
    println!("Set up {}, with a fresh identity.", path.display());
    println!();
    println!("This Mac's public key — paste it as `agent_public_key` on the");
    println!("matching [[targets]] entry in the gateway's remotex.toml:");
    println!();
    println!("    {public_key}");
    println!();
    println!("Then paste the gateway's own key — `remotex rxa-pubkey` prints it —");
    println!("into Settings. Until you do, this agent refuses every connection.");
    println!();
    println!("The rest is in the menu bar, under the remotex-agent icon:");
    println!();
    println!("    Settings…          — the two public keys, and where the gateway's goes");
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
    public_key: bool,
    import_private_key: bool,
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
                "--public-key" => parsed.public_key = true,
                // Deliberately takes no value: the key arrives on stdin. As an
                // argument it would be in the shell's history and in `ps` for
                // every user on the machine, which is the whole of why the agent
                // has no key operations on the command line otherwise.
                "--import-private-key" => parsed.import_private_key = true,
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
      --public-key     Print this Mac's public key and exit, without serving.
                       Creates the config first if there is none, so a fresh Mac
                       can be paired over SSH. The private key is never printed
      --import-private-key
                       Replace this Mac's identity with a private key read from
                       stdin, keeping every other setting, then print the
                       resulting public key and exit. For giving a re-imaged or
                       replacement Mac the identity its gateways already know,
                       so nothing has to be re-paired. The key is taken from
                       stdin rather than an argument so it stays out of shell
                       history and out of `ps`:
                           pbpaste | remotex-agent --import-private-key
  -h, --help           Show this help
  -V, --version        Show the version

Pairing is two public keys, one each way. This Mac's goes on the gateway as
`agent_public_key` on the matching [[targets]] entry in remotex.toml; the
gateway's own — from `remotex rxa-pubkey` — goes in Settings here. Neither is a
secret; the private key behind each stays on the machine that made it.

Everything else is in the menu bar: those two keys, the listen address, the
display, the config file, the log, the two permissions, Start at Login, and Quit.
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
        assert!(parse(&["--public-key"]).unwrap().public_key);
        assert!(parse(&["--import-private-key"]).unwrap().import_private_key);
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
        for flag in [
            "--config",
            "--no-register",
            "--no-menu",
            "--public-key",
            "--import-private-key",
            "--help",
            "--version",
        ] {
            assert!(USAGE.contains(flag), "usage does not mention {flag}");
        }
    }
}
