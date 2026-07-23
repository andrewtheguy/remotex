use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rdpweb", version, about = "Browser-based RDP client")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the web server
    Serve {
        /// Host/interface to bind the web server to
        #[arg(long, env = "RDPWEB_HOST", default_value = "127.0.0.1")]
        host: String,

        /// Port to bind the web server to
        #[arg(long, env = "RDPWEB_PORT", default_value_t = 52380)]
        port: u16,

        /// RDP target host (placeholder until Phase 1 wires up the RDP engine)
        #[arg(long, env = "RDPWEB_RDP_HOST", default_value = "127.0.0.1")]
        rdp_host: String,

        /// RDP target port
        #[arg(long, env = "RDPWEB_RDP_PORT", default_value_t = 3389)]
        rdp_port: u16,
    },
}
