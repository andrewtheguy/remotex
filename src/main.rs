use anyhow::Context;
use clap::Parser;
use log::{info, warn};
use rdpweb::cli::{Cli, Commands};
use rdpweb::config::AppConfig;
use rdpweb::server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load a local `.env` if present (dev convenience), then the installed
    // config at <prefix>/etc/rdpweb.env, so RDPWEB_* (incl. credentials) can
    // live in a file instead of the shell. dotenvy never overrides already-set
    // variables, so real environment variables take precedence over both.
    let _ = dotenvy::dotenv();
    if let Some(env_file) = rdpweb::config::default_env_file() {
        dotenvy::from_path(&env_file)
            .with_context(|| format!("failed to load env file {}", env_file.display()))?;
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Install the ring crypto provider as the process default. ironrdp-tls builds
    // its rustls ClientConfig with `ClientConfig::builder()`, which requires a
    // process-wide default provider to be installed first.
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            host,
            port,
            rdp_host,
            rdp_port,
            rdp_username,
            rdp_password,
            rdp_domain,
            rdp_width,
            rdp_height,
            rdp_security,
            static_dir,
        } => {
            let config = AppConfig {
                host,
                port,
                rdp_host,
                rdp_port,
                rdp_username,
                rdp_password,
                rdp_domain,
                rdp_width,
                rdp_height,
                rdp_security,
                static_dir: static_dir.unwrap_or_else(rdpweb::config::default_static_dir),
            };
            serve(config).await?;
        }
    }

    Ok(())
}

async fn serve(config: AppConfig) -> anyhow::Result<()> {
    // Surface a misconfigured static path before we start listening. The SPA
    // handler still 404s per-request; this just makes the cause obvious.
    if !config.static_dir.is_dir() {
        warn!(
            "static dir {} not found — the web UI will 404 (set --static-dir / RDPWEB_STATIC_DIR)",
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
    info!("RDP target: {}:{}", config.rdp_host, config.rdp_port);

    axum::serve(listener, app).await.context("server error")?;
    Ok(())
}
