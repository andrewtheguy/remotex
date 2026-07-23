mod assets;
mod cli;
mod config;
mod error;
mod protocol;
mod rdp;
mod server;
mod ws;

use anyhow::Context;
use clap::Parser;
use cli::{Cli, Commands};
use config::AppConfig;
use log::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            host,
            port,
            rdp_host,
            rdp_port,
        } => {
            let config = AppConfig {
                host,
                port,
                rdp_host,
                rdp_port,
            };
            serve(config).await?;
        }
    }

    Ok(())
}

async fn serve(config: AppConfig) -> anyhow::Result<()> {
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
        "RDP target (placeholder, not yet connected): {}:{}",
        config.rdp_host, config.rdp_port
    );

    axum::serve(listener, app).await.context("server error")?;
    Ok(())
}
