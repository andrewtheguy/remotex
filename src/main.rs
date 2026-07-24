use anyhow::Context;
use clap::Parser;
use log::{info, warn};
use remotex::cli::{Cli, Commands};
use remotex::config::AppConfig;
use remotex::server;

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
        Commands::Serve { config, target } => {
            // All configuration comes from the TOML file — no env vars, no .env
            // (see src/config.rs for why).
            let (file, path) = remotex::config::load(config.as_deref())?;
            info!("config: {}", path.display());
            let config = file.resolve(target.as_deref())?;
            serve(config).await?;
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

async fn serve(config: AppConfig) -> anyhow::Result<()> {
    // Surface a misconfigured static path before we start listening. The SPA
    // handler still 404s per-request; this just makes the cause obvious.
    if !config.static_dir.is_dir() {
        warn!(
            "static dir {} not found — the web UI will 404 (set static_dir under [server])",
            config.static_dir.display()
        );
    } else if !config.static_dir.join("index.html").is_file() {
        warn!(
            "no index.html in static dir {} — the web UI will 404",
            config.static_dir.display()
        );
    }

    let app = server::router(config.clone());

    let addr = if config.host.contains(':') {
        format!("[{}]:{}", config.host, config.port)
    } else {
        format!("{}:{}", config.host, config.port)
    };

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind to {addr}"))?;
    info!("listening on http://{addr}");
    info!(
        "target {:?}: {}:{} ({:?})",
        config.target.name, config.target.host, config.target.port, config.target.protocol
    );
    info!("web login: user {:?}", config.site_passwd.username());

    axum::serve(listener, app).await.context("server error")?;
    Ok(())
}
