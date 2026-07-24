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

    /// Generate a [server].site_passwd credential for the web login: prompts
    /// for a password and prints username:bcrypt_hash
    GenPasswd {
        /// Username for the web login (must not contain ':')
        username: String,
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
        let Commands::Serve { config, target } = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(config.as_deref(), Some(std::path::Path::new("/etc/x.toml")));
        assert_eq!(target.as_deref(), Some("win"));
    }

    #[test]
    fn serve_selectors_are_optional() {
        let cli = Cli::try_parse_from(["rdpweb", "serve"]).unwrap();
        let Commands::Serve { config, target } = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert!(config.is_none() && target.is_none());
    }

    #[test]
    fn gen_passwd_takes_a_username() {
        let cli = Cli::try_parse_from(["rdpweb", "gen-passwd", "andrew"]).unwrap();
        let Commands::GenPasswd { username } = cli.command else {
            panic!("expected the gen-passwd subcommand");
        };
        assert_eq!(username, "andrew");

        // The username is required.
        assert!(Cli::try_parse_from(["rdpweb", "gen-passwd"]).is_err());
    }
}
