use anyhow::Context;
use clap::Parser;
use log::{info, warn};
use remotex::cli::{Cli, Commands};
use remotex::config::{AppConfig, Protocol};
use remotex::server;
use rxa_proto::key;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Install the ring crypto provider as the process default. ironrdp-tls builds
    // its rustls ClientConfig with `ClientConfig::builder()`, which requires a
    // process-wide default provider to be installed first.
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { config } => {
            // All configuration comes from the TOML file — no env vars, no .env
            // (see src/config.rs for why). Every target is served; the browser
            // picks one after login.
            let (file, path) = remotex::config::load(config.as_deref())?;
            info!("config: {}", path.display());
            // Said once at startup because it is the value an operator needs
            // when adding a Mac, and there is nowhere else to read it off a
            // running server — the browser never sees it (see src/server.rs).
            if file.targets.iter().any(|t| t.protocol == Protocol::Rxa) {
                info!("rxa: gateway public key {}", rxa_pubkey(&file.rxa.private_key)?);
            }
            let config = file.resolve()?;
            serve(config).await?;
        }
        Commands::ServeEmbedded { instance_dir } => {
            serve_embedded(&remotex::embedded::Instance::new(instance_dir)).await?;
        }
        Commands::CheckConfig { config, embedded } => {
            let audience = if embedded {
                remotex::config::Audience::Embedded
            } else {
                remotex::config::Audience::Served
            };
            // The message is the product here: this subcommand exists to be run by
            // the app's configuration editor and have its stderr shown to somebody
            // about to fix the file. `{:#}` keeps the whole `anyhow` chain, which
            // is what names the target the complaint is about.
            if let Err(e) = remotex::embedded::check(
                &remotex::embedded::read_candidate(config.as_deref())?,
                audience,
            ) {
                eprintln!("{e:#}");
                std::process::exit(1);
            }
        }
        Commands::GenPasswd { username } => gen_passwd(&username)?,
        // Only this half is minted here. The public half is derived on demand
        // (`rxa-pubkey`) rather than stored beside it: two copies of one fact
        // in a config file is one of them going stale.
        Commands::GenKey => println!("{}", key::generate_private(key::Role::Gateway)),
        Commands::RxaPubkey { config } => {
            // Reads `[rxa].private_key` alone, not the whole config: pairing is
            // a cycle otherwise, since a target's `agent_public_key` comes from
            // a Mac that is waiting on the value this prints.
            let (private, _) = remotex::config::load_rxa_private_key(config.as_deref())?;
            println!("{}", rxa_pubkey(&private)?);
        }
    }

    Ok(())
}

/// This gateway's `rxa` public key, from the private key in its config.
///
/// The error names the fix rather than the field, because the only way to reach
/// it is a config that has not been given an identity yet.
fn rxa_pubkey(private_key: &str) -> anyhow::Result<String> {
    let private = private_key.trim();
    anyhow::ensure!(
        !private.is_empty(),
        "[rxa].private_key is unset — generate one with `remotex gen-key`"
    );
    key::public_text_of(key::Role::Gateway, private).context("invalid [rxa].private_key")
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

/// Run the gateway `remotex.app` started, and stop when the app does.
///
/// Three ways out, and the first is the one the guarantee rests on: the app's end
/// of our stdin closing, which happens however the app ended — see
/// [`remotex::embedded::parent_closed`]. The signal handler is for a run started by
/// hand, and the server arm only completes by failing.
async fn serve_embedded(instance: &remotex::embedded::Instance) -> anyhow::Result<()> {
    tokio::select! {
        result = remotex::embedded::serve(instance) => result?,
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
    //
    // Only for a gateway that has a web root at all: `None` is not a path that went
    // missing, it is an embedded gateway that ships no SPA — and it never reaches
    // this function anyway (see `serve_embedded`).
    if let Some(static_dir) = config.static_dir.as_deref() {
        if !static_dir.is_dir() {
            warn!(
                "static dir {} not found — the web UI will 404 (set static_dir under [server])",
                static_dir.display()
            );
        } else if !static_dir.join("index.html").is_file() {
            warn!(
                "no index.html in static dir {} — the web UI will 404",
                static_dir.display()
            );
        }
    }

    let app = server::router(config.clone());

    let addr = if config.host.contains(':') {
        format!("[{}]:{}", config.host, config.port)
    } else {
        format!("{}:{}", config.host, config.port)
    };

    // **Every** address the host resolves to, not the first one.
    //
    // `host = "localhost"` is the case that made this necessary: it resolves to
    // both `::1` and `127.0.0.1`, `TcpListener::bind` takes whichever the resolver
    // returned first (on macOS, `::1`), and the other loopback is then simply
    // refused. The startup line said `listening on http://localhost:52675`, which
    // is exactly the wrong thing to print when only half of localhost answers — a
    // native client on `127.0.0.1` failed with `NSURLErrorCannotConnectToHost` and
    // nothing in the log hinted why.
    //
    // Binding each of them is also what makes "both loopbacks" expressible at all.
    // `::` would do it by accident — a dual-stack wildcard reaches `127.0.0.1` — but
    // it is `0.0.0.0` and `::/0` together, i.e. every interface on the machine,
    // which is not what somebody asking for localhost is asking for.
    //
    // A literal is unaffected: `127.0.0.1`, `::1` and `0.0.0.0` each resolve to
    // themselves and bind exactly one socket, as before.
    let listeners = bind_all(&resolved_addrs(&addr).await?, &addr)?;
    for listener in &listeners {
        if let Ok(socket) = listener.local_addr() {
            info!("listening on http://{socket}");
        }
    }
    info!("{} target(s) available in the post-login picker:", config.targets.len());
    for target in &config.targets {
        info!(
            "  target {:?}: {}:{} ({:?})",
            target.name, target.host, target.port, target.protocol
        );
    }
    if let remotex::auth::GatewayAuth::Login(site_passwd) = &config.auth {
        info!("web login: user {:?}", site_passwd.username());
    }

    // One server per listener over the same router — `Router` is `Clone`, and the
    // session slot behind it is a single `Arc`, so which socket a browser arrived on
    // is invisible from here. That matters: two listeners are two doors to one
    // gateway, not two gateways.
    let mut servers = tokio::task::JoinSet::new();
    for listener in listeners {
        // `bind_all` takes the sockets synchronously so the all-or-nothing check
        // needs no runtime and is testable on its own; tokio wants them
        // non-blocking before it will drive them.
        listener
            .set_nonblocking(true)
            .context("cannot make a listening socket non-blocking")?;
        let listener = tokio::net::TcpListener::from_std(listener)
            .context("cannot hand a listening socket to the runtime")?;
        servers.spawn(axum::serve(listener, app.clone()).into_future());
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
    Ok(())
}

/// Take a listening socket on every address, or none at all.
///
/// **A port already in use is fatal, on any one of them.** It means something else
/// is serving that port — most often a gateway from an earlier run — and starting
/// beside it is worse than not starting: a browser resolving `localhost` picks
/// either family, so it would reach the old process or the new one depending on
/// which address it happened to try, and the two would fight over the agent's
/// session slot. This is the "stale gateway answered while the fresh one thought
/// it was serving" failure, and refusing to start is the only honest answer.
///
/// The one tolerated failure is an address family this machine does not have:
/// `localhost` resolves to `::1` on a host with IPv6 switched off, and refusing to
/// start over a loopback that cannot exist would be useless. It is warned about,
/// and it is still fatal if it leaves nothing bound.
///
/// All-or-nothing rather than a preflight probe, because a probe is a lie by the
/// time it returns: whatever it found free can be taken in the microseconds before
/// the real bind. Holding the sockets *is* the check, and dropping the ones already
/// taken on the way out leaves every port exactly as it was found.
fn bind_all(
    addrs: &[std::net::SocketAddr],
    addr: &str,
) -> anyhow::Result<Vec<std::net::TcpListener>> {
    let mut listeners = Vec::new();
    for &socket in addrs {
        match std::net::TcpListener::bind(socket) {
            Ok(listener) => listeners.push(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
                warn!(
                    "not listening on {socket}: {e} — this machine has no such \
                     address, which is what an IPv6 name resolves to on a host \
                     with IPv6 disabled"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // `listeners` drops here, releasing anything already taken.
                anyhow::bail!(
                    "{socket} is already in use — something else is serving that \
                     port (an earlier `remotex serve`?). Stop it first; starting \
                     beside it would leave two gateways answering {addr} \
                     unpredictably"
                );
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot listen on {socket}"));
            }
        }
    }
    anyhow::ensure!(
        !listeners.is_empty(),
        "none of the addresses {addr} resolves to can be listened on"
    );
    Ok(listeners)
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
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    /// Both loopbacks at one port, which is what `host = "localhost"` resolves to
    /// and the reason `bind_all` exists.
    fn both_loopbacks(port: u16) -> Vec<SocketAddr> {
        vec![
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        ]
    }

    /// A port nothing is listening on, found by taking one and letting it go.
    ///
    /// Racy in principle and fine in practice: the window is microseconds and the
    /// alternative is a hardcoded port, which is racy against every other test run
    /// on the machine rather than against nothing in particular.
    fn free_port() -> u16 {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn both_loopbacks_are_bound_for_one_name() {
        let port = free_port();
        let listeners = bind_all(&both_loopbacks(port), "localhost:0").unwrap();
        let bound: Vec<_> = listeners
            .iter()
            .map(|l| l.local_addr().unwrap())
            .collect();
        assert_eq!(bound, both_loopbacks(port), "both, in the order resolved");
    }

    /// The check the whole function is for: a port held on *any* resolved address
    /// stops the start, even when the other address is free.
    #[test]
    fn a_port_already_in_use_on_one_address_refuses_the_start() {
        let port = free_port();
        // Hold IPv4 only, leaving the IPv6 loopback free — the half-bound shape
        // that used to start happily and answer on one family.
        let squatter = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();

        let err = bind_all(&both_loopbacks(port), "localhost:0")
            .expect_err("a port in use must refuse the start");
        let text = format!("{err:#}");
        assert!(text.contains("already in use"), "{text}");
        assert!(text.contains(&port.to_string()), "it must name the port: {text}");

        // And nothing was left holding the address that *was* free: the IPv6
        // listener taken on the way through has to be released, or a retry after
        // stopping the other process would fail against ourselves.
        drop(squatter);
        bind_all(&both_loopbacks(port), "localhost:0")
            .expect("the refused attempt must not have kept a socket");
    }

    /// An address this machine does not have is warned about, not fatal — that is
    /// what `::1` is on a host with IPv6 disabled. Simulated with an address no
    /// machine has assigned.
    #[test]
    fn an_unavailable_address_is_skipped_while_the_rest_still_bind() {
        let port = free_port();
        let mut addrs = vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), port)];
        addrs.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));

        let listeners = bind_all(&addrs, "example:0").expect("the loopback still binds");
        assert_eq!(listeners.len(), 1);
        assert_eq!(
            listeners[0].local_addr().unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
        );
    }

    /// ...unless it leaves nothing at all, which is a gateway nobody can reach.
    #[test]
    fn an_address_nothing_could_bind_is_still_fatal() {
        let addrs = vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            free_port(),
        )];
        let err = bind_all(&addrs, "example:0").expect_err("nothing bound is fatal");
        assert!(format!("{err:#}").contains("can be listened on"), "{err:#}");
    }
}
