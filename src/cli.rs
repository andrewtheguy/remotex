use clap::{Parser, Subcommand};

use crate::config::Security;

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

        /// RDP target host
        #[arg(long, env = "RDPWEB_RDP_HOST", default_value = "127.0.0.1")]
        rdp_host: String,

        /// RDP target port
        #[arg(long, env = "RDPWEB_RDP_PORT", default_value_t = 3389)]
        rdp_port: u16,

        /// RDP username (server-side; never sent to the browser)
        #[arg(long, env = "RDPWEB_RDP_USERNAME", default_value = "")]
        rdp_username: String,

        /// RDP password (server-side; never sent to the browser)
        #[arg(long, env = "RDPWEB_RDP_PASSWORD", default_value = "", hide_env_values = true)]
        rdp_password: String,

        /// RDP domain (optional)
        #[arg(long, env = "RDPWEB_RDP_DOMAIN")]
        rdp_domain: Option<String>,

        /// Initial desktop width to request
        #[arg(long, env = "RDPWEB_RDP_WIDTH", default_value_t = 1280)]
        rdp_width: u16,

        /// Initial desktop height to request
        #[arg(long, env = "RDPWEB_RDP_HEIGHT", default_value_t = 800)]
        rdp_height: u16,

        /// RDP security mode: auto (TLS+NLA), nla (NLA only), or tls (no NLA)
        #[arg(long, env = "RDPWEB_RDP_SECURITY", value_enum, default_value_t = Security::Auto)]
        rdp_security: Security,
    },
}
