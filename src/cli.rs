use std::path::PathBuf;

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
        #[arg(
            long,
            env = "RDPWEB_RDP_HOST",
            value_parser = clap::builder::NonEmptyStringValueParser::new()
        )]
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

        /// Directory of the built frontend to serve. Defaults to the installed
        /// `share/rdpweb/web` next to the binary, or `frontend/dist` in a checkout.
        #[arg(long, env = "RDPWEB_STATIC_DIR")]
        static_dir: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::Cli;

    #[test]
    fn rdp_host_is_required() {
        let command = Cli::command();
        let serve = command
            .get_subcommands()
            .find(|command| command.get_name() == "serve")
            .expect("serve subcommand");
        let rdp_host = serve
            .get_arguments()
            .find(|argument| argument.get_id() == "rdp_host")
            .expect("rdp_host argument");

        assert!(rdp_host.is_required_set());
    }

    #[test]
    fn rdp_host_cannot_be_empty() {
        assert!(Cli::try_parse_from(["rdpweb", "serve", "--rdp-host", ""]).is_err());
    }
}
