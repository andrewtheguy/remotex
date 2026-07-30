//! The command line, as clap derives it.
//!
//! Every doc comment below is also this binary's `--help` text, which is why TOML
//! table syntax and `<prefix>` placeholders sit in them unquoted: backticks would be
//! printed literally on somebody's terminal. rustdoc reads `[server]` as a link and
//! `<prefix>` as an HTML tag and complains about both, so its two lints are off here
//! rather than the help text being bent to suit a renderer nobody reads it in.
#![allow(rustdoc::broken_intra_doc_links, rustdoc::invalid_html_tags)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "remotex", version, about = "Browser-based RDP client")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the web server. Every [[targets]] profile is served; the browser
    /// picks one after login (there is no --target selector).
    Serve {
        /// TOML config file (default: the installed <prefix>/etc/remotex.toml;
        /// required when running from a checkout)
        #[arg(short, long)]
        config: Option<PathBuf>,
    },

    /// Generate a [server].site_passwd credential for the web login: prompts
    /// for a password and prints username:bcrypt_hash
    GenPasswd {
        /// Username for the web login (must not contain ':')
        username: String,
    },

    /// Generate this gateway's "rxa" identity: prints a private key to paste
    /// into [rxa].private_key. Its public half is `remotex rxa-pubkey`.
    GenKey,

    /// Print this gateway's "rxa" public key, derived from [rxa].private_key:
    /// the value to add to each Mac agent's authorized_gateways list
    RxaPubkey {
        /// TOML config file (default: the installed <prefix>/etc/remotex.toml;
        /// required when running from a checkout)
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands};

    #[test]
    fn serve_parses_config() {
        let cli = Cli::try_parse_from(["remotex", "serve", "-c", "/etc/x.toml"]).unwrap();
        let Commands::Serve { config } = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(config.as_deref(), Some(std::path::Path::new("/etc/x.toml")));
    }

    #[test]
    fn serve_config_is_optional() {
        let cli = Cli::try_parse_from(["remotex", "serve"]).unwrap();
        let Commands::Serve { config } = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert!(config.is_none());
    }

    #[test]
    fn serve_rejects_the_removed_target_selector() {
        // Target selection is browser-side now: --target is gone.
        assert!(Cli::try_parse_from(["remotex", "serve", "--target", "win"]).is_err());
    }

    #[test]
    fn gen_passwd_takes_a_username() {
        let cli = Cli::try_parse_from(["remotex", "gen-passwd", "andrew"]).unwrap();
        let Commands::GenPasswd { username } = cli.command else {
            panic!("expected the gen-passwd subcommand");
        };
        assert_eq!(username, "andrew");

        // The username is required.
        assert!(Cli::try_parse_from(["remotex", "gen-passwd"]).is_err());
    }

    #[test]
    fn gen_key_takes_no_arguments() {
        let cli = Cli::try_parse_from(["remotex", "gen-key"]).unwrap();
        assert!(matches!(cli.command, Commands::GenKey));
        // Simpler than gen-passwd on purpose: no prompt, no username. And no
        // config either — a fresh identity does not depend on one.
        assert!(Cli::try_parse_from(["remotex", "gen-key", "mac"]).is_err());
    }

    // The public key is *derived*, so unlike gen-key this one has to be told
    // which config holds the private key it comes from.
    #[test]
    fn rxa_pubkey_takes_the_same_config_flag_as_serve() {
        let cli = Cli::try_parse_from(["remotex", "rxa-pubkey", "-c", "/etc/x.toml"]).unwrap();
        let Commands::RxaPubkey { config } = cli.command else {
            panic!("expected the rxa-pubkey subcommand");
        };
        assert_eq!(config.as_deref(), Some(std::path::Path::new("/etc/x.toml")));

        let cli = Cli::try_parse_from(["remotex", "rxa-pubkey"]).unwrap();
        let Commands::RxaPubkey { config } = cli.command else {
            panic!("expected the rxa-pubkey subcommand");
        };
        assert!(config.is_none(), "defaults to the installed config");
    }
}
