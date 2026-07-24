use std::path::PathBuf;

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
        /// TOML config file (default: the installed <prefix>/etc/rdpweb.toml;
        /// required when running from a checkout)
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Name of the [[targets]] profile to serve (default: the first one)
        #[arg(short, long)]
        target: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands};

    #[test]
    fn serve_parses_config_and_target() {
        let cli =
            Cli::try_parse_from(["rdpweb", "serve", "-c", "/etc/x.toml", "--target", "win"])
                .unwrap();
        let Commands::Serve { config, target } = cli.command;
        assert_eq!(config.as_deref(), Some(std::path::Path::new("/etc/x.toml")));
        assert_eq!(target.as_deref(), Some("win"));
    }

    #[test]
    fn serve_selectors_are_optional() {
        let cli = Cli::try_parse_from(["rdpweb", "serve"]).unwrap();
        let Commands::Serve { config, target } = cli.command;
        assert!(config.is_none() && target.is_none());
    }
}
