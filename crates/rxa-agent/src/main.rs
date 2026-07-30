//! macOS RXA agent entry point.
//!
//! AppKit and cursor reads stay on the main thread while the socket runtime,
//! ScreenCaptureKit callbacks and *startup itself* run elsewhere — the main thread
//! is given to AppKit as soon as the status item exists, so the menu answers while
//! the agent is still coming up (see `start_up` and `menubar::Starting::pump_until`).
//! The menu-bar app requires Screen Recording and Accessibility, persists one
//! keypair, and serves one authenticated gateway connection at a time.

// A bare `#![cfg(target_os = "macos")]` would compile the crate away to
// nothing on Linux and fail at link time with "main function not found",
// which says nothing useful. Fail at compile time with the reason instead.
#[cfg(not(target_os = "macos"))]
compile_error!(
    "rxa-agent is macOS-only (ScreenCaptureKit + CoreGraphics). It is excluded \
     from the workspace's default-members; build it on a Mac with \
     `cargo build -p rxa-agent`."
);

mod authorized;
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
use log::{debug, error, info, warn};
use tokio::time::timeout;

/// How often the main thread re-reads the system cursor when `--no-menu` has
/// taken the run loop away. The menu bar polls at the same rate from a timer.
const CURSOR_POLL: Duration = Duration::from_millis(100);
const LOG_FILE_BYTES: usize = 5 * 1024 * 1024;
const LOG_FILE_BACKUPS: usize = 3;

fn main() -> anyhow::Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;

    // Answered before the login item, the bind and the TCC prompts: this is a
    // question about the config file, not a launch, and asking it must not
    // register anything, take the port from a running agent, or raise a prompt.
    //
    // `load_or_create`, so the first thing anyone does over SSH on a fresh Mac
    // mints the identity and prints it in one go. And `?` rather than the
    // persistent degraded GUI below: a question asked from a shell has no menu
    // bar and must answer failure with a nonzero exit instead of silence.
    if args.public_key {
        let _ = init_logging();
        let (config, _, _) = config::load_or_create(args.config.as_deref())?;
        println!("{}", config.public_key());
        return Ok(());
    }

    // Also answered before anything else, and for the same reason as the two
    // below: this changes launchd, not this process. It must not bind the port or
    // raise a TCC prompt on the way.
    //
    // It exists as a flag as well as a menu item because a test VM is reached over
    // SSH, where there is no menu to click — and because installing a login item
    // from a terminal is how anyone would expect to do it once.
    if args.install_launchagent || args.uninstall_launchagent {
        let _ = init_logging();
        if args.uninstall_launchagent {
            loginitem::uninstall()?;
            println!("removed the login item ({})", loginitem::plist_path()?.display());
        } else {
            loginitem::install()?;
            println!(
                "{} now starts at login, from {}",
                loginitem::LABEL,
                loginitem::program()?.display()
            );
        }
        return Ok(());
    }

    // Same reasoning, and the same place in the launch: a config edit, not a
    // launch.
    if args.import_private_key {
        let _ = init_logging();
        let key = read_private_key_from_stdin()?;
        let config = config::import_private_key(args.config.as_deref(), &key)?;
        // The public half, because that is the value to check against whatever
        // the gateway already has — the point of importing rather than minting.
        println!("{}", config.public_key());
        return Ok(());
    }

    // The status item is the application shell, not a reward for successful
    // startup. Put it on screen before registration, config I/O, binding,
    // permission probes, virtual-display setup or the network runtime can fail.
    let starting = (!args.no_menu).then(menubar::Starting::new);
    let log_path = init_logging();

    // The status item goes up first and AppKit gets the main thread immediately after,
    // so startup runs on a worker. An item whose run loop has not started is on
    // screen and unclickable — the worst of both — and the whole reason it goes up
    // before the work is so a launch that wedges can still be quit. See
    // `menubar::Starting::pump_until`.
    //
    // `state` and `tracker` are made here rather than in the worker: both are needed
    // by the network thread and by the menu, and the tracker reads AppKit's cursor,
    // which belongs to this thread.
    let tracker = Arc::new(cursor::Tracker::new());
    let state = Arc::new(state::AgentState::new());
    let no_menu = args.no_menu;
    let (finished, startup) = std::sync::mpsc::channel();
    let worker = {
        let tracker = Arc::clone(&tracker);
        let state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("rxa-startup".to_owned())
            .spawn(move || {
                let _ = finished.send(start_up(args, tracker, state));
            })
    };
    if let Err(e) = worker {
        let body = format!("Could not start the startup worker: {e}");
        return Err(startup_error(
            starting,
            "remotex-agent could not start",
            &body,
            e.into(),
            None,
        ));
    }

    let outcome = match starting.as_ref() {
        Some(starting) => starting.pump_until(&startup),
        // `--no-menu` has no interface to keep responsive, so there is nothing to
        // pump: wait for the answer the way any other program would.
        None => match startup.recv() {
            Ok(outcome) => outcome,
            Err(e) => return Err(anyhow::anyhow!("startup did not report back: {e}")),
        },
    };

    let ready = match outcome {
        // The registered job is running this bundle now; this copy is done.
        Startup::StoodDown => return Ok(()),
        Startup::Failed {
            title,
            body,
            error,
            settings,
        } => {
            // The menu that is left behind can offer Settings when there is a config
            // to edit — which is the difference between "that port is in use" being a
            // dead end and being a thing you fix from the menu bar.
            let degraded = settings.map(|settings| menubar::Degraded {
                settings,
                state,
                tracker,
                log_path,
            });
            return Err(startup_error(starting, &title, &body, error, degraded));
        }
        Startup::Ready(ready) => ready,
    };

    if no_menu {
        // No run loop, so nothing drives an NSTimer: poll the pointer shape the
        // plain way instead. Sessions read the same cache either way.
        loop {
            tracker.poll();
            std::thread::sleep(CURSOR_POLL);
        }
    }

    // Hands the main thread to AppKit and never returns.
    menubar::run(
        starting.expect("a GUI launch creates its status item before startup"),
        state,
        tracker,
        ready.settings,
        log_path,
        ready.screen_recording_at_launch,
        ready.owned,
    )
}

/// What startup produced for the menu bar.
struct Ready {
    settings: Arc<settings::Settings>,
    /// Whether Screen Recording was effective *at launch* — see `report_permissions`.
    screen_recording_at_launch: bool,
    /// The display the agent made, for the settings dialog to name.
    owned: Option<capture::Target>,
}

/// How a launch ended, as the worker reports it to the main thread.
///
/// A failure travels rather than being presented where it happened: the panel and
/// the degraded menu are AppKit, and AppKit belongs to the thread that is busy
/// keeping the menu bar answering.
enum Startup {
    Ready(Ready),
    Failed {
        title: String,
        body: String,
        error: anyhow::Error,
        /// The config, once there is one to edit. What makes the degraded menu's
        /// **Settings…** work — see [`menubar::Degraded`].
        settings: Option<Arc<settings::Settings>>,
    },
    /// This copy handed over to the registered job and has nothing left to do.
    StoodDown,
}

/// Everything between the status item appearing and the agent serving, off the main
/// thread.
///
/// Ordering inside is unchanged and still deliberate — register, hand over, read the
/// config, bind, ask for permissions, make the display, start the runtime — because
/// each step's failure has to leave the ones before it done. What changed is only
/// where it runs and how a failure gets out: [`Startup::Failed`] instead of a call
/// into AppKit.
fn start_up(
    args: Args,
    tracker: Arc<cursor::Tracker>,
    state: Arc<state::AgentState>,
) -> Startup {
    macro_rules! fail {
        ($title:expr, $body:expr, $error:expr) => {
            fail!($title, $body, $error, None)
        };
        ($title:expr, $body:expr, $error:expr, $settings:expr) => {
            return Startup::Failed {
                title: ($title).to_owned(),
                body: $body,
                error: $error,
                settings: $settings,
            }
        };
    }

    // Which copy is this? Logged on every launch because the answer used to be
    // invisible and cost an afternoon: a bundle's `--version` and the version in
    // this log both describe the *file*, and say nothing about which file launchd
    // is starting. Now the log says.
    match loginitem::program() {
        Ok(exe) => info!("agent: running from {}", exe.display()),
        Err(e) => warn!("agent: cannot tell where this executable lives: {e:#}"),
    }

    // Nothing here registers, installs or rewrites a login item — that happens only
    // when somebody asks, from the menu's Start at Login or `--install-launchagent`
    // (see `loginitem`). A copy of the app that merely runs must never change what
    // launchd starts, which is exactly how a copy opened from a mounted disk image
    // used to capture the job and leave every later `kickstart` starting it.
    //
    // What *is* worth saying on every launch is a mismatch, which is the failure
    // that hid: the login item naming some other copy.
    if let loginitem::Status::Elsewhere(other) = loginitem::status() {
        warn!(
            "login item: {} starts a different copy at login: {} — `launchctl kickstart` \
             will start that one, not this. Re-tick Start at Login from the copy you want.",
            loginitem::LABEL,
            other.display()
        );
    }

    // Is this process the launchd job, or a copy of the app somebody opened? A
    // normal `open` on a Mac where the job is already running hands execution over
    // rather than racing it for the port.
    //
    // Done only after creating the tray so even a slow handoff is visibly in
    // progress; the job that takes over creates its own tray before it does any
    // work too. `--no-handover` is for running it by hand in a terminal while
    // developing, where standing down is the opposite of what is wanted.
    let job = Job::read();
    if !args.no_menu && !args.no_handover && !job.is_this_process() && hand_over_to_launchd(job) {
        return Startup::StoodDown;
    }

    let loaded = config::load_or_create(args.config.as_deref());
    let (config, path, created) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => {
            // No number of restarts fixes a config file. Leave the tray up in a
            // degraded state so the failure remains visible and Quit remains
            // reachable.
            let body =
                format!("{e:#}\n\nFix the config file, then quit and reopen remotex-agent.");
            fail!("remotex-agent could not start", body, e);
        }
    };

    // Read here rather than at the handshake, and then never again this run: which
    // gateways are allowed in is settled for the process's lifetime, exactly like
    // the config, and for the same reason — the alternative is a list that changes
    // under a live session. A save re-execs (see [`crate::settings`]).
    let authorized_path = authorized::path_beside(&path);
    let authorized = match authorized::Authorized::load(&authorized_path) {
        Ok(authorized) => Arc::new(authorized),
        Err(e) => {
            // A hand-edited list nobody can parse is the same shape of failure as a
            // broken config, and it takes the same answer: stay up so the menu can
            // fix it, rather than restarting into it forever.
            let body = format!(
                "{e:#}\n\nFix the authorized gateways file, then quit and reopen \
                 remotex-agent."
            );
            fail!("remotex-agent could not start", body, e);
        }
    };

    // Built as soon as both files parse, before anything that can fail with them in
    // hand: a bind that finds the port taken is the failure whose fix is a *setting*,
    // and the degraded menu can only offer Settings if this exists by then.
    let settings = settings::Settings::new(
        config.clone(),
        path.clone(),
        (*authorized).clone(),
        authorized_path,
    );

    info!(
        "remotex-agent {} — rxa/{}, config {}",
        env!("CARGO_PKG_VERSION"),
        rxa_proto::VERSION,
        path.display()
    );
    if created {
        info!(
            "config: created {} with a fresh identity, no gateways authorized yet",
            path.display()
        );
    }

    // Only worth printing where somebody can read it. The public key *is* in it
    // now — it is the next thing to do, and it is not a secret; the private key
    // stays out of log files, out of shell history and out of a terminal
    // somebody may be screen-sharing.
    if created && std::io::stdout().is_terminal() {
        print_first_run(&path, &config.public_key());
    }

    // Bind on the main thread after login-item repair but before TCC prompts, so
    // an existing instance produces a visible error without prompting.
    let addr = config
        .socket_addr()
        .expect("listen validated in Config::validate");
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let body = format!(
                "{} is already in use by another process.\n\nIf that process is another copy \
                 of remotex-agent, its icon is in the menu bar at the top of the screen. \
                 Otherwise, pick another port in Settings — it is in this agent's menu.",
                config.listen
            );
            fail!(
                "remotex-agent cannot listen",
                body,
                e.into(),
                Some(Arc::clone(&settings))
            );
        }
        Err(e) => {
            let body = format!(
                "{} could not be bound: {e}\n\nChange the listen address in Settings, in \
                 this agent's menu.",
                config.listen
            );
            fail!(
                "remotex-agent cannot listen",
                body,
                e.into(),
                Some(Arc::clone(&settings))
            );
        }
    };
    info!("agent: listening on {}", config.listen);
    // tokio adopts it below, and only a non-blocking socket can be driven by a
    // reactor.
    if let Err(e) = listener.set_nonblocking(true) {
        let body = format!("{} could not be made non-blocking: {e}", config.listen);
        fail!(
                "remotex-agent cannot listen",
                body,
                e.into(),
                Some(Arc::clone(&settings))
            );
    }

    // Keep the pre-request Screen Recording state: granting it in the system
    // prompt below changes TCC immediately, but capture only works after this
    // process is relaunched. The menu must not mistake that new TCC value
    // for a permission effective in this launch.
    let screen_recording_at_launch = report_permissions();


    // The socket runs on its own thread so the main thread stays free for
    // AppKit, which owns the menu bar and the pointer shape both.
    let serve_tracker = Arc::clone(&tracker);
    let serve_state = Arc::clone(&state);
    let keys = Keys {
        private: config.private_key_bytes(),
        authorized: Arc::clone(&authorized),
    };
    if keys.authorized.is_empty() {
        warn!(
            "no authorized gateways: this Mac will refuse every connection. Its \
             public key is {} — paste that into the gateway's agent_public_key, and \
             the gateway's own key into Settings > Authorized gateways.",
            config.public_key()
        );
    } else {
        // Named at startup, because this is the one place the list is read and the
        // log is where somebody goes when a gateway they expected to work does not.
        info!(
            "agent: {} authorized gateway(s): {}",
            keys.authorized.len(),
            keys.authorized
                .entries()
                .iter()
                .map(|entry| entry.name().unwrap_or("(unnamed)").to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // The optional display is process-owned and must outlive every session.
    // Failure leaves the Mac's physical displays available.
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
    // Restore the pointer if macOS moved it onto the newly created display.
    if let Some(display) = virtual_display.as_ref() {
        input::PointerHome::for_new_display(display.id()).restore();
    }
    // Creation adds a selectable display; it does not select or arrange it.
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
    engage_hidpi(owned.handle.clone());
    let serve_owned = owned.clone();
    let keep_ui_on_failure = !args.no_menu;

    let network = std::thread::Builder::new()
        .name("rxa-net".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    let error = format!("Cannot build the network runtime: {e}");
                    error!("agent: {error}");
                    if keep_ui_on_failure {
                        serve_state.failed(error);
                        return;
                    }
                    std::process::exit(1);
                }
            };
            match runtime.block_on(serve(
                listener,
                keys,
                serve_owned,
                serve_tracker,
                Arc::clone(&serve_state),
            )) {
                Ok(()) => {
                    let error = "The network service stopped unexpectedly".to_owned();
                    error!("agent: {error}");
                    if keep_ui_on_failure {
                        serve_state.failed(error);
                    } else {
                        std::process::exit(0);
                    }
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    error!("agent: {error}");
                    if keep_ui_on_failure {
                        serve_state.failed(error);
                    } else {
                        std::process::exit(1);
                    }
                }
            }
        });
    if let Err(e) = network {
        let body = format!("Could not start the network worker: {e}");
        fail!(
            "remotex-agent could not start",
            body,
            e.into(),
            Some(Arc::clone(&settings))
        );
    }

    Startup::Ready(Ready {
        settings,
        screen_recording_at_launch,
        owned: owned.target,
    })
}

/// Settle the new display's density on a thread of its own.
///
/// Spawned rather than called, because the work is *waiting for the main thread to
/// get out of the way*: a `CGVirtualDisplay` publishes no mode and accepts no
/// reconfigure until the main queue is being served, which here means after this
/// function returns and the run loop starts. The argument for all of that, and for
/// why it cannot live in `VirtualDisplay::create`, is in
/// [`virtualdisplay::VirtualDisplay::engage_hidpi`].
///
/// Takes the shared handle rather than the display, so a client's `HostScale`
/// landing in the same second waits its turn instead of racing this.
fn engage_hidpi(handle: Option<Arc<std::sync::Mutex<virtualdisplay::VirtualDisplay>>>) {
    let Some(handle) = handle else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("rxa-hidpi".to_owned())
        .spawn(move || {
            let engaged = handle
                .lock()
                .map_err(|_| anyhow::anyhow!("the display lock is poisoned"))
                .and_then(|display| display.engage_hidpi());
            if let Err(e) = engaged {
                warn!(
                    "virtualdisplay: {e:#} — the desktop is soft until a client on a Retina \
                     screen asks for 2x"
                );
            }
        });
    if let Err(e) = spawned {
        warn!("virtualdisplay: cannot check the new display's density: {e}");
    }
}

/// Keep GUI startup failures in the degraded menu, or return them to a headless
/// caller.
///
/// [`menubar::Starting::fail`] never returns, while `--no-menu` has no
/// [`menubar::Starting`] and receives the underlying error for a nonzero exit.
fn startup_error(
    starting: Option<menubar::Starting>,
    title: &str,
    body: &str,
    error: anyhow::Error,
    degraded: Option<menubar::Degraded>,
) -> anyhow::Error {
    warn!("{title}: {body}");
    if let Some(starting) = starting {
        starting.fail(title, body, degraded);
    }
    error
}

/// Restart the login-item job after a config change. launchd avoids AppKit state
/// surviving an `exec` and listener races between parent and replacement.
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

/// What launchd says about the registered job, from one `launchctl print`.
///
/// Two startup steps turn on this answer and must not be taken by the job itself:
/// handing over ([`hand_over_to_launchd`]) and refreshing the plist launchd holds
/// ([`loginitem::refresh`]). Asking twice could get two different answers, so it is
/// asked once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    /// launchd has no record of the label: never registered on this Mac, or booted
    /// out of this domain. There is nothing to hand over to.
    Unknown,
    /// launchd has the job. The pid is `None` when it is loaded but not running.
    Known(Option<u32>),
}

impl Job {
    fn read() -> Self {
        let service = format!("gui/{}/{}", uid(), loginitem::LABEL);
        let Ok(printed) = std::process::Command::new("/bin/launchctl")
            .args(["print", &service])
            .output()
        else {
            return Job::Unknown;
        };
        if !printed.status.success() {
            return Job::Unknown;
        }
        Job::Known(
            String::from_utf8_lossy(&printed.stdout)
                .lines()
                .find_map(|line| line.trim().strip_prefix("pid = ")?.trim().parse::<u32>().ok()),
        )
    }

    /// Whether the running job *is* this process.
    fn is_this_process(self) -> bool {
        self == Job::Known(Some(std::process::id()))
    }
}

/// Hand execution to the registered LaunchAgent. This keeps one TCC-authorized
/// instance and avoids bind races.
///
/// `job` is [`Job::read`]'s answer, taken once by the caller — which has also
/// already established that the job is not this process.
fn hand_over_to_launchd(job: Job) -> bool {
    if job == Job::Unknown {
        return false;
    }
    let service = format!("gui/{}/{}", uid(), loginitem::LABEL);
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

/// Log to stderr when interactive, otherwise to bounded per-user files.
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

/// This Mac's identity and the gateways it answers.
///
/// `authorized` is empty on a first launch, and empty is what a Mac nobody has
/// authorized anything on has: it still listens, so the port is visibly answering
/// and the menu bar is there to add a key from, but no connection gets past the
/// handshake. `Arc` rather than `Copy`, because the list is a `Vec` shared with
/// every concurrent handshake and never changes within a run — a change is a
/// restart (see [`crate::settings`]).
#[derive(Clone)]
struct Keys {
    private: [u8; 32],
    authorized: Arc<authorized::Authorized>,
}

/// A connection as a log line names it: the authorized list's name for that
/// gateway with its address beside it, or the address alone for an entry with no
/// comment.
fn who(authenticated: &session::Authenticated, peer: SocketAddr) -> String {
    match authenticated.gateway() {
        Some(name) => format!("{name} ({peer})"),
        None => peer.to_string(),
    }
}

/// Accept gateway connections. Handshakes and claims run concurrently, and only
/// a *granted claim* moves the single session slot — authentication alone earns a
/// connection the right to ask for it (see [`session::Authenticated`] and
/// [`state::decide`]).
///
/// The two layers are deliberately separate. Whether a peer may be here at all is
/// settled by the keys, in the handshake; whose turn it is to be here is settled
/// by the session id it claims with. Collapsing them — the shape this loop had
/// when it evicted on a completed handshake — makes "one active session" and "one
/// permitted gateway" the same sentence, which they are not (see
/// `docs/roadmap.md`).
async fn serve(
    listener: std::net::TcpListener,
    keys: Keys,
    owned: session::Owned,
    tracker: Arc<cursor::Tracker>,
    state: Arc<state::AgentState>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| anyhow::anyhow!("cannot drive the listening socket: {e}"))?;

    // The task serving whoever holds the slot. Who that *is* lives in `state`,
    // which the menu bar reads too — one source of truth for the session id, the
    // address and the elapsed time, all three of which the decision below needs.
    let mut current: Option<tokio::task::JoinHandle<()>> = None;
    // Connections that have finished a handshake and stated their claim, waiting
    // for that claim to be judged. Bounded, and generously: it holds only
    // authenticated peers, and the loop below drains it immediately.
    let (ready_tx, mut ready_rx) =
        tokio::sync::mpsc::channel::<(SocketAddr, session::Authenticated, session::Claim)>(4);

    loop {
        tokio::select! {
            // A connection that proved itself and asked for the slot. *Now* the
            // slot may move — see the module docs on why this is not done at
            // accept, and not at the handshake either.
            Some((peer, authenticated, claim)) = ready_rx.recv() => {
                let now = Instant::now();
                let held = state.current();
                match state::decide(held.as_ref().map(|c| c.session), claim.session, claim.force) {
                    state::Decision::Refuse => {
                        // Unwrapped safely: `Refuse` is only reachable with a
                        // holder, and it is the same read `decide` just judged.
                        let (holder, held_secs) = held
                            .as_ref()
                            .expect("a refusal has an incumbent to name")
                            .holder(now);
                        info!(
                            "agent: refusing {peer} — {holder} has held the session for \
                             {held_secs}s (the client can take over)"
                        );
                        // Off the accept loop, like the handshake: this writes to
                        // a peer that may be slow, and the session in the slot is
                        // not waiting on it.
                        tokio::spawn(async move {
                            if let Err(e) = authenticated.refuse(holder, held_secs).await {
                                debug!("agent: could not tell {peer} the slot is taken: {e:#}");
                            }
                        });
                        continue;
                    }
                    // Named off the authorized list where the entry carried one,
                    // because "which gateway is this" is now a real question.
                    state::Decision::Take => info!("agent: {} connected", who(&authenticated, peer)),
                    // Not a second client, so not an eviction worth alarming
                    // anyone about: this is a link that dropped, a target
                    // switched, or a browser taken over on the gateway.
                    state::Decision::Reclaim => info!(
                        "agent: {} connected — the same session reconnecting",
                        who(&authenticated, peer)
                    ),
                    state::Decision::TakeOver => info!(
                        "agent: {} connected — taking the session over, as its client asked",
                        who(&authenticated, peer)
                    ),
                }

                // Recorded before the eviction, so the menu bar never blinks
                // through a "not connected" state during a reconnect. The id is
                // what keeps the evicted session from clearing this one on its
                // way out.
                let id = state.connected(
                    claim.session,
                    authenticated.gateway().map(str::to_owned),
                    peer,
                    now,
                );
                if let Some(previous) = current.take() {
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
                // With nothing on the list there is no key to judge anyone by, so
                // this is as far as any connection gets. Checked here rather than
                // left to the lookup so the log says *why* — an empty list is a
                // Mac nobody has authorized yet, not a gateway that got it wrong.
                if keys.authorized.is_empty() {
                    warn!(
                        "agent: refusing {peer} — no gateways are authorized \
                         (Settings > Authorized gateways)"
                    );
                    drop(stream);
                    continue;
                }

                // Off the accept path, and on a clock. Handshaking inline would
                // let one peer that connects and then says nothing block every
                // later connection — including the real gateway's — for as long
                // as it cared to hold the socket open. The claim is read under
                // the same deadline, and for the same reason: a peer that
                // authenticates and then goes quiet must not hold anything up
                // either.
                let ready_tx = ready_tx.clone();
                let keys = keys.clone();
                tokio::spawn(async move {
                    let opening = async {
                        let mut authenticated =
                            session::handshake(stream, keys.private, &keys.authorized).await?;
                        let claim = authenticated.claim().await?;
                        Ok::<_, anyhow::Error>((authenticated, claim))
                    };
                    match timeout(HANDSHAKE_TIMEOUT, opening).await {
                        Ok(Ok((authenticated, claim))) => {
                            // A full queue means four authenticated peers are
                            // already waiting, which one gateway at a time
                            // cannot produce. Dropping this one is right.
                            if ready_tx.try_send((peer, authenticated, claim)).is_err() {
                                warn!("agent: dropping {peer}, too many pending sessions");
                            }
                        }
                        // Never fatal to the agent, and never visible to the
                        // session already running: an unpaired peer, a port
                        // scanner, or a gateway on another protocol version.
                        Ok(Err(e)) => warn!("agent: refusing {peer}: {e:#}"),
                        Err(_) => warn!(
                            "agent: refusing {peer}: no handshake and claim within {}s",
                            HANDSHAKE_TIMEOUT.as_secs()
                        ),
                    }
                });
            }
        }
    }
}

/// Request both TCC grants and return whether Screen Recording was already
/// effective at launch. Screen capture must be requested explicitly;
/// unauthorized input injection fails silently.
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

/// Read an imported private key from stdin so it never appears in argv.
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
/// mostly how to start — which config to read, whether to put up a menu, and
/// whether to stand down for the launchd job. `clap` would be a dependency for
/// that. The two `--*-launchagent` flags are the exception, and exist because
/// installing a login item is a thing to do from a terminal on a VM as well as
/// from a menu.
#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    no_handover: bool,
    no_menu: bool,
    public_key: bool,
    import_private_key: bool,
    install_launchagent: bool,
    uninstall_launchagent: bool,
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
                "--no-handover" => parsed.no_handover = true,
                "--install-launchagent" => parsed.install_launchagent = true,
                "--uninstall-launchagent" => parsed.uninstall_launchagent = true,
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

With no options: create the config if absent and serve. Normally launched by
macOS at login rather than by hand — see --install-launchagent, which is the only
thing that arranges that. A launch never registers anything by itself.

Options:
  -c, --config <path>  Config file (default:
                       ~/Library/Application Support/remotex-agent/config.toml)
      --no-handover    Serve even when launchd already has a copy running, instead
                       of standing down for it (for development)
      --install-launchagent
                       Write ~/Library/LaunchAgents/dev.remotex.agent.plist naming
                       *this* executable by absolute path, load it, and exit. This
                       is what makes the agent start at login, and the same thing
                       the menu's Start at Login does. Refused from a mounted disk
                       image, where the path would be gone by the next login
      --uninstall-launchagent
                       Unload and remove that plist, and exit. The agent then runs
                       only when somebody opens it
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
        // Standing down for a running launchd job is the default: opening the app
        // when it is already running must not race it for the port.
        assert!(!args.no_handover);
        // And a launch installs nothing. This is the property that stops a copy
        // opened from a disk image capturing what launchd starts, so it is pinned
        // here as well as in `loginitem`.
        assert!(!args.install_launchagent && !args.uninstall_launchagent);
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
        assert!(parse(&["--no-handover"]).unwrap().no_handover);
        assert!(parse(&["--install-launchagent"]).unwrap().install_launchagent);
        assert!(
            parse(&["--uninstall-launchagent"])
                .unwrap()
                .uninstall_launchagent
        );
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
            "--no-handover",
            "--install-launchagent",
            "--uninstall-launchagent",
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
