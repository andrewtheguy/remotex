//! A managed local gateway: one browser instance, one loopback port, one token.
//!
//! `remotex serve-embedded --instance-dir <dir> --web-root <dir>` is not a
//! deployment. It is started by an instance manager, serves that browser instance
//! alone, and dies with its parent. Everything that makes a `serve` gateway
//! configurable is therefore decided here instead: the port is whatever the kernel
//! gives, the address is `127.0.0.1`, the SPA comes from a caller-provided web root,
//! and there is no login to offer — see
//! [`crate::config::Audience::Embedded`].
//!
//! The SPA it serves is the same browser client as `remotex serve`. The token below
//! stands in for the login: the manager puts it in the browser's cookie store, so
//! the page authenticates itself the way any logged-in browser does and the manager
//! needs no session protocol of its own.
//!
//! Two pipes carry the whole arrangement, in opposite directions, and neither one
//! carries the other's job:
//!
//! - **stdout, once**: the [`Handshake`] line, printed after the socket is bound so
//!   the port in it is a fact rather than an intention. It is how the parent learns
//!   both the port and the token, and it is the only thing this process ever writes
//!   to stdout — logging goes to stderr for the parent to retain.
//! - **stdin, never**: nothing is written to it in either direction. It exists so
//!   this process can notice that its parent is gone; see [`parent_closed`].
//!
//! The token is handed over that way rather than through `argv` (which `ps` shows
//! to every process on the machine), the environment (inherited by anything either
//! side spawns), or a file (which outlives the process that made it). A pipe's read
//! end belongs to this process and its write end to the parent, and both disappear
//! when they do.

use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::path::PathBuf;

use anyhow::Context as _;
use log::info;
use serde::{Deserialize, Serialize};

use crate::auth::EmbeddedToken;
use crate::config::{Audience, ConfigFile};

/// The line this process prints on stdout once, before serving.
///
/// Serialized as JSON rather than as two lines of text so the parent can tell a
/// complete handshake from a truncated one by parsing it, which matters because the
/// alternative — reading fields until they run out — cannot distinguish "still
/// coming" from "this build does not send it".
#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub struct Handshake {
    /// The loopback port the kernel gave us, and the only one the parent may use.
    pub port: u16,
    /// The token the manager puts in the browser's `remotex_session` cookie, which
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
    /// a handshake that could not be built is a gateway the parent cannot reach, so
    /// the error is returned rather than swallowed into an empty line.
    pub fn line(&self) -> anyhow::Result<String> {
        let json = serde_json::to_string(self).context("cannot encode the handshake")?;
        Ok(format!("{json}\n"))
    }
}

/// The managed instance directory: its config and future per-instance state.
///
/// Everything about an embedded gateway is under here, and nothing outside it is
/// read — in particular not the installed gateway's global config, which belongs
/// to the server install. Multiple instances can therefore coexist without being
/// able to change what another does, which is the point of naming the
/// directory on the command line rather than deriving it from the executable's
/// location the way [`crate::config::installed_config_path`] does.
#[derive(Clone, Debug)]
pub struct Instance {
    dir: PathBuf,
}

impl Instance {
    /// Name an instance directory. Does not touch the filesystem: the parent owns
    /// creating the directory and writing any config template.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `<dir>/remotex.toml` — the one file a user edits.
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("remotex.toml")
    }

    /// Read and check this instance's config.
    ///
    /// The error carries the path so a manager can put the right file in front of
    /// somebody who is about to edit it.
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
/// bound is a promise this process might not keep, and the parent would race a
/// connection against a listener that does not exist yet.
pub async fn serve(instance: &Instance, web_root: PathBuf) -> anyhow::Result<()> {
    let file = instance.load()?;
    let token = EmbeddedToken::generate();
    let config = file.resolve_embedded(token.clone(), web_root)?;

    // Before the bind so a missing page is reported before the handshake. This path
    // is supplied by the launcher rather than the config, so name that half in the
    // error.
    crate::config::warn_if_no_web_root(
        &config.static_dir,
        "the launcher-provided --web-root is incomplete",
    );

    // One socket on one address, and that address comes from the config rather than
    // from a literal here — `resolve_embedded` is where it is decided, and two places
    // saying `127.0.0.1` is one of them going stale the day the other changes.
    //
    // `serve` binds every address its host name resolves to, for reasons that do not
    // apply here: the client is told a single port on a single address by the line
    // below, so there is no name to resolve and no second family for it to arrive on.
    // Always TCP here: `resolve_embedded` decides this address, and an embedded
    // config may not carry a `[server]` block to argue with it. The refusal is for
    // the day that stops being true, because a browser cannot address a
    // socket file.
    let crate::config::ListenAddr::Tcp(addr) = &config.listen else {
        anyhow::bail!("the embedded gateway listens on loopback TCP, which its client addresses by URL");
    };
    let listener =
        TcpListener::bind(addr.as_str()).with_context(|| format!("cannot listen on {addr}"))?;
    let local = listener
        .local_addr()
        .context("cannot read the port the kernel gave us")?;

    let handshake = Handshake {
        port: local.port(),
        token: token.as_str().to_owned(),
    };
    // Written and flushed before the runtime is handed the socket: the parent is
    // blocked on this line, and a buffered stdout would deadlock the pair of us.
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(handshake.line()?.as_bytes())
        .and_then(|()| stdout.flush())
        .context("cannot write the handshake to stdout")?;
    drop(stdout);

    // The bound socket rather than the configured address: the port there is 0.
    info!("embedded gateway listening on http://{local}");
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
    axum::serve(crate::server::NodelayListener(listener), app)
        .await
        .context("server error")?;
    Ok(())
}

/// Resolve when the process that started us is gone.
///
/// Nothing is ever sent on stdin, and that is what makes this work: the read cannot
/// return data, so the only thing it can return is end-of-file — which happens when
/// the last write end of the pipe closes. The parent holds that write end for
/// exactly as long as it is alive, so this fires however the parent exits, without
/// requiring its shutdown code to run.
///
/// This is the layer the "the gateway always stops with its manager" guarantee
/// rests on. It is portable across the platforms a future manager may run on.
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
                // EOF: the write end is gone, so the parent is gone.
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
    // signal handlers and the parent's own termination still stop this process.
    if rx.await.is_err() {
        std::future::pending::<()>().await
    }
}

/// Validate candidate config text the way an embedded gateway will read it.
///
/// An instance manager can call this through the binary on unsaved editor text, so
/// what it accepts is by construction what the gateway accepts. A second parser in
/// a manager would be a second opinion, and the one that mattered would be whichever
/// ran last.
pub fn check(text: &str) -> anyhow::Result<()> {
    let file = ConfigFile::parse_with(text, Audience::Embedded)?;
    // Parsing alone would accept a file the gateway then refuses to start on, so
    // the check goes all the way through resolution. The web root is the
    // launcher's to name and is not in the file, so any path is sufficient here.
    file.resolve_embedded(EmbeddedToken::generate(), PathBuf::new())
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handshake is one line, and it survives the round trip a parent makes.
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
        // The field names the parent reads, spelled out so renaming one here fails
        // here rather than at launch.
        assert!(line.contains("\"port\":49213"), "{line}");
        assert!(line.contains("\"token\":\"abc-123\""), "{line}");
    }

    #[test]
    fn an_instance_names_its_config_beside_itself() {
        let instance = Instance::new("/tmp/inst");
        assert_eq!(instance.config_path(), PathBuf::from("/tmp/inst/remotex.toml"));
    }

    /// An embedded config is `[branding]` and `[[targets]]`, and an empty one is a
    /// new instance rather than an error.
    #[test]
    fn the_embedded_audience_accepts_a_config_with_nothing_in_it() {
        check("").expect("a first launch has no targets yet");
        let served = crate::config::check("").expect_err("a served gateway needs a target");
        assert!(format!("{served:#}").contains("[[targets]]"), "{served:#}");
    }

    /// The block the launcher owns is refused where it cannot mean anything, with a
    /// message that says which keys belong instead.
    #[test]
    fn the_embedded_audience_refuses_a_server_block() {
        for text in ["[server]\n", "[server]\nlisten = \"0.0.0.0:1234\"\n"] {
            let error = check(text).expect_err("[server] is the launcher's");
            let message = format!("{error:#}");
            assert!(message.contains("[server]"), "{message}");
            assert!(message.contains("[[targets]]"), "it must say what does belong: {message}");
        }
    }

    /// One table, one place, both audiences — including an embedded instance, whose
    /// config has no `[server]` block a name could have lived in.
    #[test]
    fn branding_is_one_top_level_table_for_both_audiences() {
        check("[branding]\ntext = \"work laptop\"\n")
            .expect("the instance names itself with the top-level table");

        let file = ConfigFile::parse_with(
            "[branding]\ntext = \"work laptop\"\nlogo = \"/tmp/logo.png\"\n",
            Audience::Embedded,
        )
        .unwrap();
        let resolved = file
            .resolve_embedded(EmbeddedToken::generate(), PathBuf::from("/w"))
            .unwrap();
        assert_eq!(resolved.branding.text, "work laptop");
        assert_eq!(resolved.branding.logo.unwrap().mime, "image/png");
        assert_eq!(resolved.static_dir, PathBuf::from("/w"), "the launcher's web root");

        // There is exactly one place to write it, so the block it used to live in
        // refuses it — `deny_unknown_fields` and nothing else, which is the whole of
        // the migration this project offers.
        let error = crate::config::check("[server]\nbranding = \"x\"\n")
            .expect_err("[server].branding is gone");
        assert!(format!("{error:#}").contains("branding"), "{error:#}");

        // And the old top-level string spelling fails to parse: a table is not a
        // string.
        let error = check("branding = \"x\"\n")
            .expect_err("the string spelling is gone");
        assert!(format!("{error:#}").contains("branding"), "{error:#}");
    }

    /// A target is a target: the same rules, the same messages, either audience.
    #[test]
    fn target_rules_do_not_depend_on_the_audience() {
        let text = "[[targets]]\nname = \"win\"\nprotocol = \"rdp\"\nhost = \"\"\n";
        for error in [
            check(text).expect_err("an empty host is an empty host"),
            crate::config::check(text).expect_err("an empty host is an empty host"),
        ] {
            assert!(format!("{error:#}").contains("empty host"), "{error:#}");
        }
    }

    /// The whole point of `check`: it refuses what the gateway would refuse to
    /// start on, not merely what fails to parse. `audio` on a VNC target is
    /// well-formed TOML and an unusable config.
    #[test]
    fn checking_goes_as_far_as_starting_would() {
        let text = "[[targets]]\nname = \"box\"\nprotocol = \"vnc\"\nhost = \"::1\"\naudio = true\n";
        let error = check(text).expect_err("audio is rejected on vnc");
        assert!(format!("{error:#}").contains("audio"), "{error:#}");
    }
}
