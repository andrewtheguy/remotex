//! The gateway inside `remotex.app`: one client, one loopback port, one token.
//!
//! `remotex serve-embedded --instance-dir <dir> --web-root <dir>` is not a
//! deployment. It is started by the macOS app, serves that app alone, and dies with
//! it. Everything that makes a `serve` gateway configurable is therefore decided
//! here instead: the port is whatever the kernel gives, the address is `127.0.0.1`,
//! the SPA comes out of the app's bundle, and there is no login to offer — see
//! [`crate::config::Audience::Embedded`].
//!
//! The SPA it serves is the same one a browser gets, and `remotex.app` shows exactly
//! that page in a web view. The token below stands in for the login: the app puts it
//! in the view's cookie store, so the page authenticates itself the way any logged-in
//! browser does and the app needs no session of its own.
//!
//! Two pipes carry the whole arrangement, in opposite directions, and neither one
//! carries the other's job:
//!
//! - **stdout, once**: the [`Handshake`] line, printed after the socket is bound so
//!   the port in it is a fact rather than an intention. It is how the app learns
//!   both the port and the token, and it is the only thing this process ever writes
//!   to stdout — logging goes to stderr, which the app keeps in `gateway.log`.
//! - **stdin, never**: nothing is written to it in either direction. It exists so
//!   this process can notice that the app is gone; see [`parent_closed`].
//!
//! The token is handed over that way rather than through `argv` (which `ps` shows
//! to every process on the machine), the environment (inherited by anything either
//! side spawns), or a file (which outlives the process that made it). A pipe's read
//! end belongs to this process and its write end to the app, and both disappear
//! when they do.

use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use log::info;
use serde::{Deserialize, Serialize};

use crate::auth::EmbeddedToken;
use crate::config::{Audience, ConfigFile};

/// The line this process prints on stdout once, before serving.
///
/// Serialized as JSON rather than as two lines of text so the app can tell a
/// complete handshake from a truncated one by parsing it, which matters because the
/// alternative — reading fields until they run out — cannot distinguish "still
/// coming" from "this build does not send it".
#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub struct Handshake {
    /// The loopback port the kernel gave us, and the only one the app may use.
    pub port: u16,
    /// The token the app puts in its web view's `remotex_session` cookie, which
    /// then carries it to every request and to the `/ws` upgrades.
    pub token: String,
}

// Manual Debug, for the reason [`EmbeddedToken`]'s is manual: this is the other
// holder of the same secret, and the one that exists to be *written somewhere*. A
// derived Debug would put the token in any log line that ever formats a handshake,
// which is a mistake nobody would notice making.
impl std::fmt::Debug for Handshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handshake")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Handshake {
    /// The line to print, newline included.
    ///
    /// Never fails in practice — a `u16` and a base64 string always serialize — and
    /// a handshake that could not be built is a gateway the app cannot reach, so
    /// the error is returned rather than swallowed into an empty line.
    pub fn line(&self) -> anyhow::Result<String> {
        let json = serde_json::to_string(self).context("cannot encode the handshake")?;
        Ok(format!("{json}\n"))
    }
}

/// The app's own instance directory: its config, its log, its preferences.
///
/// Everything about an embedded gateway is under here, and nothing outside it is
/// read — in particular not `<prefix>/etc/remotex.toml`, which belongs to the
/// server install. The two can therefore sit on one Mac without either being able
/// to change what the other does, which is the point of naming the directory on the
/// command line rather than deriving it from the executable's location the way
/// [`crate::config::installed_config_path`] does.
#[derive(Clone, Debug)]
pub struct Instance {
    dir: PathBuf,
}

impl Instance {
    /// Name an instance directory. Does not touch the filesystem: the app creates
    /// the directory and writes the config template, because it is the half that
    /// can show somebody the result.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `<dir>/remotex.toml` — the one file a user edits.
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("remotex.toml")
    }

    /// Read and check this instance's config.
    ///
    /// The error carries the path, because the app puts this message in front of
    /// somebody who is about to edit that file.
    pub fn load(&self) -> anyhow::Result<ConfigFile> {
        let path = self.config_path();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        ConfigFile::parse_with(&text, Audience::Embedded)
            .with_context(|| format!("in config file {}", path.display()))
    }
}

/// Serve an instance until something stops us, printing the handshake to `stdout`.
///
/// The order is the contract: bind, *then* announce. A port announced before it is
/// bound is a promise this process might not keep, and the app would race a
/// connection against a listener that does not exist yet.
pub async fn serve(instance: &Instance, web_root: PathBuf) -> anyhow::Result<()> {
    let file = instance.load()?;
    let token = EmbeddedToken::generate();
    let config = file.resolve_embedded(token.clone(), web_root)?;

    // One socket on one address, and that address comes from the config rather than
    // from a literal here — `resolve_embedded` is where it is decided, and two places
    // saying `127.0.0.1` is one of them going stale the day the other changes.
    //
    // `serve` binds every address its host name resolves to, for reasons that do not
    // apply here: the client is told a single port on a single address by the line
    // below, so there is no name to resolve and no second family for it to arrive on.
    let listener = TcpListener::bind((config.host.as_str(), config.port))
        .with_context(|| format!("cannot listen on {}", config.host))?;
    let port = listener
        .local_addr()
        .context("cannot read the port the kernel gave us")?
        .port();

    let handshake = Handshake {
        port,
        token: token.as_str().to_owned(),
    };
    // Written and flushed before the runtime is handed the socket: the app is
    // blocked on this line, and a buffered stdout would deadlock the pair of us.
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(handshake.line()?.as_bytes())
        .and_then(|()| stdout.flush())
        .context("cannot write the handshake to stdout")?;
    drop(stdout);

    info!("embedded gateway listening on http://{}:{port}", config.host);
    info!("config: {}", instance.config_path().display());
    info!("web root: {}", config.static_dir.display());
    info!("{} target(s) available in the picker:", config.targets.len());
    for target in &config.targets {
        info!(
            "  target {:?}: {}:{} ({:?})",
            target.name, target.host, target.port, target.protocol
        );
    }

    let app = crate::server::router(config);
    listener
        .set_nonblocking(true)
        .context("cannot make the listening socket non-blocking")?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .context("cannot hand the listening socket to the runtime")?;
    axum::serve(listener, app).await.context("server error")?;
    Ok(())
}

/// Resolve when the process that started us is gone.
///
/// Nothing is ever sent on stdin, and that is what makes this work: the read cannot
/// return data, so the only thing it can return is end-of-file — which happens when
/// the last write end of the pipe closes. The app holds that write end for exactly
/// as long as it is alive, so this fires on a clean quit, a crash, a Force Quit and
/// a `SIGKILL` alike, without any code of the app's having to run.
///
/// This is the layer the "the gateway always stops with the app" guarantee rests
/// on. macOS has no `PR_SET_PDEATHSIG` to ask the kernel for the same thing, and
/// polling `getppid` would leave a window in which an orphan is still listening.
///
/// On a `serve-embedded` run started by hand from a terminal, stdin is that
/// terminal and this simply never fires — which is the right answer for a run
/// nobody is parenting.
pub async fn parent_closed() {
    // A blocking read on a dedicated thread, not `tokio::io::stdin`: this future is
    // one arm of a `select!`, and the losing arm is dropped. Tokio's stdin is
    // backed by a blocking pool task that cannot be cancelled mid-read, so a
    // dropped read there can hold the runtime open at shutdown. A detached thread
    // that outlives the process by a few microseconds costs nothing.
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        loop {
            match std::io::stdin().read(&mut byte) {
                // EOF: the write end is gone, so the app is gone.
                Ok(0) => break,
                // Nothing sends anything on this pipe, so a byte can only be
                // somebody driving this by hand — ignored rather than treated as a
                // signal, since a stray keystroke in a terminal must not stop a
                // gateway.
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // A broken pipe or a closed descriptor means the same thing as EOF.
                Err(_) => break,
            }
        }
        let _ = tx.send(());
    });
    // The sender is only dropped without sending if that thread panics, which would
    // mean stdin cannot be read at all. Never firing is the safe direction: the
    // signal handlers and the app's own terminate still stop this process.
    if rx.await.is_err() {
        std::future::pending::<()>().await
    }
}

/// Validate candidate config text the way an embedded gateway will read it.
///
/// The app's configuration panel calls this — through the binary, on the text in
/// the editor, before anything is written — so that what it accepts is by
/// construction what the gateway accepts. A second parser in Swift would be a
/// second opinion, and the one that mattered would be whichever ran last.
pub fn check(text: &str, audience: Audience) -> anyhow::Result<()> {
    let file = ConfigFile::parse_with(text, audience)?;
    // Parsing alone would accept a file the gateway then refuses to start on, so
    // the check goes all the way through resolution — for the served audience that
    // is where the login credential is validated.
    match audience {
        // The web root is the app's to name and is not in the file, so checking
        // text says nothing about it: any path resolves, and whether it holds an
        // SPA is a question about the bundle rather than about this config.
        Audience::Embedded => file
            .resolve_embedded(EmbeddedToken::generate(), PathBuf::new())
            .map(|_| ()),
        Audience::Served => file.resolve().map(|_| ()),
    }
}

/// Read config text from a file, or from stdin when no path is given.
///
/// Stdin is the app's way in: the text being checked is what is in an editor that
/// has not been saved, so there is no file to name yet.
pub fn read_candidate(path: Option<&Path>) -> anyhow::Result<String> {
    match path {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display())),
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("failed to read the config from stdin")?;
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handshake is one line, and it survives the round trip the app makes.
    #[test]
    fn a_handshake_is_one_parseable_line() {
        let handshake = Handshake {
            port: 49213,
            token: "abc-123".to_owned(),
        };
        let line = handshake.line().unwrap();
        assert!(line.ends_with('\n'), "{line:?}");
        assert_eq!(line.matches('\n').count(), 1, "exactly one line: {line:?}");
        assert_eq!(
            serde_json::from_str::<Handshake>(line.trim_end()).unwrap(),
            handshake
        );
        // The field names the app reads, spelled out so renaming one here fails
        // here rather than at launch.
        assert!(line.contains("\"port\":49213"), "{line}");
        assert!(line.contains("\"token\":\"abc-123\""), "{line}");
    }

    #[test]
    fn an_instance_names_its_config_beside_itself() {
        let instance = Instance::new("/tmp/inst");
        assert_eq!(instance.config_path(), PathBuf::from("/tmp/inst/remotex.toml"));
    }

    /// The app's config is `branding` and `[[targets]]`, and an empty one is a first
    /// launch rather than an error.
    #[test]
    fn the_embedded_audience_accepts_a_config_with_nothing_in_it() {
        check("", Audience::Embedded).expect("a first launch has no targets yet");
        let served = check("", Audience::Served).expect_err("a served gateway needs a target");
        assert!(format!("{served:#}").contains("[[targets]]"), "{served:#}");
    }

    /// The block the app owns is refused where it cannot mean anything, with a
    /// message that says which keys belong instead.
    #[test]
    fn the_embedded_audience_refuses_a_server_block() {
        for text in ["[server]\n", "[server]\nport = 1234\n"] {
            let error = check(text, Audience::Embedded).expect_err("[server] is the app's");
            let message = format!("{error:#}");
            assert!(message.contains("[server]"), "{message}");
            assert!(message.contains("[[targets]]"), "it must say what does belong: {message}");
        }
    }

    /// One key, one place, both audiences — including the app, whose config has no
    /// `[server]` block a name could have lived in.
    #[test]
    fn branding_is_one_top_level_key_for_both_audiences() {
        check("branding = \"work laptop\"\n", Audience::Embedded)
            .expect("the app names itself with the top-level key");

        let file = ConfigFile::parse_with("branding = \"work laptop\"\n", Audience::Embedded)
            .unwrap();
        let resolved = file
            .resolve_embedded(EmbeddedToken::generate(), PathBuf::from("/w"))
            .unwrap();
        assert_eq!(resolved.branding, "work laptop");
        assert_eq!(resolved.static_dir, PathBuf::from("/w"), "the app's bundle");

        // There is exactly one place to write it, so the block it used to live in
        // refuses it — `deny_unknown_fields` and nothing else, which is the whole of
        // the migration this project offers.
        let error = check("[server]\nbranding = \"x\"\n", Audience::Served)
            .expect_err("[server].branding is gone");
        assert!(format!("{error:#}").contains("branding"), "{error:#}");
    }

    /// A target is a target: the same rules, the same messages, either audience.
    #[test]
    fn target_rules_do_not_depend_on_the_audience() {
        let text = "[[targets]]\nname = \"win\"\nprotocol = \"rdp\"\nhost = \"\"\n";
        for audience in [Audience::Embedded, Audience::Served] {
            let error = check(text, audience).expect_err("an empty host is an empty host");
            assert!(format!("{error:#}").contains("empty host"), "{error:#}");
        }
    }

    /// The whole point of `check`: it refuses what the gateway would refuse to
    /// start on, not merely what fails to parse. `audio` on a VNC target is
    /// well-formed TOML and an unusable config.
    #[test]
    fn checking_goes_as_far_as_starting_would() {
        let text = "[[targets]]\nname = \"box\"\nprotocol = \"vnc\"\nhost = \"::1\"\naudio = true\n";
        let error = check(text, Audience::Embedded).expect_err("audio is rejected on vnc");
        assert!(format!("{error:#}").contains("audio"), "{error:#}");
    }
}
