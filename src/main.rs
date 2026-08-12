use anyhow::Context;
use clap::Parser;
use log::{info, warn};
use remotex::cli::{Cli, Commands};
use remotex::config::{AppConfig, ListenAddr};
use remotex::server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { config, listen } => {
            // The listen address is the one thing a deployment says outside the
            // file — `--listen`, or `REMOTEX_LISTEN` for a container that has an
            // environment but no argv to edit. Everything else comes from the
            // TOML file, credentials included (see src/config.rs for why). Every
            // target is served; the browser picks one after login.
            let (file, path) = remotex::config::load(config.as_deref())?;
            info!("config: {}", path.display());
            let config = file.resolve_with(listen.as_deref())?;
            serve(config).await?;
        }
        #[cfg(feature = "embedded-gateway")]
        Commands::Tui {
            port,
            instances_dir,
            web_root,
        } => {
            anyhow::ensure!(port != 0, "--port must be between 1 and 65535");
            remotex::embedded::run_tui(remotex::embedded::TuiOptions {
                port,
                instances_dir: instances_dir
                    .map(Ok)
                    .unwrap_or_else(remotex::embedded::default_instances_dir)?,
                web_root: web_root.unwrap_or_else(remotex::config::default_static_dir),
            })
            .await?;
        }
        #[cfg(feature = "embedded-gateway")]
        Commands::ServeEmbedded {
            instance_dir,
            web_root,
        } => {
            serve_embedded(&remotex::embedded::Instance::new(instance_dir), web_root).await?;
        }
        Commands::CheckConfig {
            config,
            #[cfg(feature = "embedded-gateway")]
            embedded,
        } => {
            // The message is the product here: an instance manager can run this for
            // its configuration editor and show stderr to somebody about to fix the
            // file. `{:#}` keeps the whole `anyhow` chain, which
            // is what names the target the complaint is about.
            let text = remotex::config::read_candidate(config.as_deref())?;
            #[cfg(feature = "embedded-gateway")]
            let result = if embedded {
                remotex::embedded::check(&text)
            } else {
                remotex::config::check(&text)
            };
            #[cfg(not(feature = "embedded-gateway"))]
            let result = remotex::config::check(&text);
            if let Err(e) = result {
                eprintln!("{e:#}");
                std::process::exit(1);
            }
        }
        Commands::GenPasswd { username } => gen_passwd(&username)?,
    }

    Ok(())
}

/// Generate the `[server].site_passwd` value: prompt for the password (hidden,
/// asked twice on a TTY; read as one line when piped) and print the encoded
/// credential to stdout, pipeable straight into the config.
fn gen_passwd(username: &str) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;

    let password = if std::io::stdin().is_terminal() {
        let password = rpassword::prompt_password("Password: ")?;
        let confirm = rpassword::prompt_password("Confirm password: ")?;
        anyhow::ensure!(password == confirm, "passwords do not match");
        password
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    };
    let encoded = remotex::auth::generate(username, &password, remotex::auth::DEFAULT_COST)?;
    println!("{encoded}");
    Ok(())
}

/// Run a managed embedded gateway, and stop when its parent does.
///
/// Three ways out, and the first is the one the guarantee rests on: the parent's
/// end of our stdin closing, which happens however the parent ended — see
/// [`remotex::embedded::parent_closed`]. The signal handler is for a run started by
/// hand, and the server arm only completes by failing.
#[cfg(feature = "embedded-gateway")]
async fn serve_embedded(
    instance: &remotex::embedded::Instance,
    web_root: std::path::PathBuf,
) -> anyhow::Result<()> {
    tokio::select! {
        // In order, and the order is the point. `serve` reads and checks the config
        // before its first `await`, so it is ready with a refusal on the very first
        // poll — while `parent_closed` can be ready on *its* first poll too, when
        // the reading thread gets to an already-closed stdin before this one gets
        // back from `spawn`. Under the unbiased default the two race, and the arm
        // that wins decides whether a refused config is reported at all: `[server]`
        // in the file, and one run in five exits 0 with nothing on stderr.
        biased;
        result = remotex::embedded::serve(instance, web_root) => result?,
        _ = remotex::embedded::parent_closed() => {
            info!("stdin closed: whatever started this gateway is gone; stopping");
        }
        _ = shutdown_signal() => info!("shutdown signal received; stopping"),
    }
    Ok(())
}

async fn serve(config: AppConfig) -> anyhow::Result<()> {
    // Surface a misconfigured static path before we start listening. The SPA
    // handler still 404s per-request; this just makes the cause obvious.
    remotex::config::warn_if_no_web_root(
        &config.static_dir,
        "set static_dir under [server]",
    );

    let app = server::router(config.clone());

    // One server per listener over the same router — `Router` is `Clone`, and the
    // session slot behind it is a single `Arc`, so which socket a browser arrived on
    // is invisible from here. That matters: two listeners are two doors to one
    // gateway, not two gateways.
    let mut servers = tokio::task::JoinSet::new();
    // Lives until this function returns, and taking the socket file away is what it
    // is for. `None` for TCP, which leaves nothing behind to clean up.
    let mut socket_file = None;

    match &config.listen {
        ListenAddr::Tcp(addr) => {
            // **Every** address the host resolves to, not the first one.
            //
            // `listen = "localhost:52380"` is the case that made this necessary: it
            // resolves to both `::1` and `127.0.0.1`, `TcpListener::bind` takes
            // whichever the resolver returned first (on macOS, `::1`), and the other
            // loopback is then simply refused. The startup line said `listening on
            // http://localhost:52675`, which is exactly the wrong thing to print when
            // only half of localhost answers — a client resolving `localhost` to
            // `127.0.0.1` was refused, and nothing in the log hinted why.
            //
            // Binding each of them is also what makes "both loopbacks" expressible at
            // all. `::` would do it by accident — a dual-stack wildcard reaches
            // `127.0.0.1` — but it is `0.0.0.0` and `::/0` together, i.e. every
            // interface on the machine, which is not what somebody asking for
            // localhost is asking for.
            //
            // A literal is unaffected: `127.0.0.1`, `::1` and `0.0.0.0` each resolve
            // to themselves and bind exactly one socket, as before.
            let listeners = server::bind_all(&resolved_addrs(addr).await?, addr)?;
            for listener in &listeners {
                if let Ok(socket) = listener.local_addr() {
                    info!("listening on http://{socket}");
                }
            }
            for listener in listeners {
                // `bind_all` takes the sockets synchronously so the all-or-nothing
                // check needs no runtime and is testable on its own; tokio wants them
                // non-blocking before it will drive them.
                listener
                    .set_nonblocking(true)
                    .context("cannot make a listening socket non-blocking")?;
                let listener = tokio::net::TcpListener::from_std(listener)
                    .context("cannot hand a listening socket to the runtime")?;
                servers.spawn(
                    axum::serve(server::NodelayListener(listener), app.clone()).into_future(),
                );
            }
        }
        ListenAddr::Unix(path) => {
            let listener = bind_unix(path)?;
            // Armed the moment the socket exists, so every way out of this function
            // takes it away with it.
            socket_file = Some(SocketFile(path.clone()));
            info!("listening on unix:{}", path.display());
            listener
                .set_nonblocking(true)
                .context("cannot make the listening socket non-blocking")?;
            let listener = tokio::net::UnixListener::from_std(listener)
                .context("cannot hand the listening socket to the runtime")?;
            // No `NodelayListener`: Nagle is a TCP algorithm, and a Unix socket has
            // none of it to switch off.
            servers.spawn(axum::serve(listener, app.clone()).into_future());
        }
    }

    info!("{} target(s) available in the post-login picker:", config.targets.len());
    for target in &config.targets {
        info!(
            "  target {:?}: {}:{} ({:?})",
            target.name, target.host, target.port, target.protocol
        );
    }
    if let Some(site_passwd) = config.auth.login() {
        info!("web login: user {:?}", site_passwd.username());
    }

    // Race the servers against an explicit shutdown signal. Relying on the OS
    // default SIGINT disposition to terminate proved flaky on macOS — Ctrl+C
    // was intermittently ignored while a detached engine thread was still
    // running, forcing a SIGKILL. An installed handler makes it deterministic.
    tokio::select! {
        // The first server to *finish* has failed — `axum::serve` only returns on
        // error — so it is reported rather than waited on for the others.
        Some(result) = servers.join_next() => {
            result.context("the server task panicked")?.context("server error")?;
        }
        _ = shutdown_signal() => info!("shutdown signal received; stopping"),
    }
    drop(socket_file);
    Ok(())
}

/// The socket file, removed when the gateway stops.
///
/// A `Drop` rather than a line at the end of `serve`, because the ways out include
/// a server that failed and a `?` on the way there. It cannot cover a `SIGKILL` — a
/// socket file outlives the process that made it — which is why [`bind_unix`] has to
/// tell a leftover from a live one rather than assume the file is always ours.
struct SocketFile(std::path::PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        // A socket that is already gone is the outcome this wanted, not a failure:
        // an operator can remove it by hand, and saying so on the way out would be
        // a warning about nothing.
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("cannot remove the socket {}: {e}", self.0.display()),
        }
    }
}

/// Take the Unix socket, replacing a leftover from a gateway that was killed but
/// never one that is still being served.
///
/// The difference is asked of the socket itself: a listener that is gone refuses a
/// connection, and one that is there accepts it. A bare `remove_file` before binding
/// would be the same mistake in file form that a preflight port probe is in socket
/// form — it would quietly evict a running gateway, whose clients then hold sockets
/// to a path nothing can be reached at again.
///
/// The mode is `0o660` rather than whatever the umask leaves: the reason to be on a
/// socket at all is that the filesystem decides who may connect, and world-writable
/// is not a decision. Owner and group, so a proxy sharing the group can reach it —
/// which is what the directory it lives in should be arranged around.
fn bind_unix(path: &std::path::Path) -> anyhow::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};

    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::ensure!(
                UnixStream::connect(path).is_err(),
                "{} is already being served by another process",
                path.display()
            );
            warn!("replacing the leftover socket {}", path.display());
            std::fs::remove_file(path)
                .with_context(|| format!("cannot remove the leftover socket {}", path.display()))?;
            UnixListener::bind(path).map_err(|e| unix_bind_error(path, e))?
        }
        Err(e) => return Err(unix_bind_error(path, e)),
    };
    // After the bind, because there is no bind that takes a mode. The window is the
    // few microseconds between the two lines, and what is behind it is a gateway
    // that still asks for a login.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("cannot set the mode of {}", path.display()))?;
    Ok(listener)
}

/// Why the socket could not be taken, with the length said out loud when that is
/// what it was.
///
/// A socket address is a fixed-size field in the kernel — 104 bytes on macOS, 108
/// on Linux — so a path under a long directory fails with `InvalidInput` and, from
/// the standard library, the words "path must be shorter than SUN_LEN". That is a
/// limit somebody hits by keeping the socket beside the config in a deep home
/// directory, and it is worth one sentence rather than a search.
fn unix_bind_error(path: &std::path::Path, e: std::io::Error) -> anyhow::Error {
    let hint = if e.kind() == std::io::ErrorKind::InvalidInput {
        format!(
            " (this path is {} bytes, and a socket address holds about 100 — \
             put the socket somewhere shorter, such as /tmp/remotex.sock)",
            path.as_os_str().len()
        )
    } else {
        String::new()
    };
    anyhow::Error::new(e).context(format!("cannot listen on unix:{}{hint}", path.display()))
}

/// Every socket address `addr` names, in the resolver's order and without
/// duplicates.
///
/// Deduplicated because a name can resolve to the same address twice — `localhost`
/// does on a machine with both an `/etc/hosts` entry and a DNS answer — and two
/// binds of one address is a spurious "address already in use" against ourselves.
async fn resolved_addrs(addr: &str) -> anyhow::Result<Vec<std::net::SocketAddr>> {
    let mut seen = Vec::new();
    for socket in tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("cannot resolve {addr}"))?
    {
        if !seen.contains(&socket) {
            seen.push(socket);
        }
    }
    anyhow::ensure!(!seen.is_empty(), "{addr} resolves to no address at all");
    Ok(seen)
}

/// Resolve when the process is asked to stop: Ctrl+C (SIGINT) on any platform,
/// or SIGTERM under a service manager on Unix. The engine threads are detached
/// and hold no state worth draining (a dropped remote session just reconnects),
/// so returning from `main` — which exits the process and reaps them — is the
/// whole shutdown: no graceful HTTP drain that a lingering WebSocket could hang.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install the Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The socket is created with a mode the filesystem can act on, which is the
    /// only reason to prefer one to a loopback port.
    #[test]
    fn a_unix_socket_is_bound_owner_and_group_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.sock");
        let listener = bind_unix(&path).expect("a fresh path binds");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660, "owner and group, and nobody else");
        assert!(
            std::os::unix::net::UnixStream::connect(&path).is_ok(),
            "and it is a socket something can reach"
        );
        drop(listener);
    }

    /// A gateway that was killed leaves its socket file behind. The next start
    /// takes it over — it is the same address, and refusing would need somebody to
    /// delete a file by hand before the service could come back.
    #[test]
    fn a_leftover_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.sock");

        // Bound and dropped without the `SocketFile` guard: exactly what a `SIGKILL`
        // leaves on disk.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "the file outlives the listener");

        let listener = bind_unix(&path).expect("a leftover is not a reason to refuse");
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        drop(listener);
    }

    /// ...but a socket something is still serving is not a leftover, and taking it
    /// would evict a running gateway whose clients could never reach it again.
    #[test]
    fn a_socket_that_is_still_served_refuses_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.sock");
        let live = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let err = bind_unix(&path).expect_err("something is already serving it");
        let text = format!("{err:#}");
        assert!(text.contains("already being served"), "{text}");
        assert!(text.contains("gateway.sock"), "it must name the socket: {text}");

        // And the live one still answers: the refusal took nothing away.
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        drop(live);
    }

    /// The socket file goes when the gateway does, so the next start is an ordinary
    /// one rather than a takeover.
    #[test]
    fn stopping_removes_the_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.sock");
        let listener = bind_unix(&path).unwrap();

        let guard = SocketFile(path.clone());
        drop(guard);
        drop(listener);
        assert!(!path.exists(), "a stopped gateway leaves no socket behind");
    }
}
