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

        /// Address to listen on — overrides [server].listen (default:
        /// 127.0.0.1:52380). Either host:port, bracketing an IPv6 literal
        /// ([::1]:52380), or unix:<path> for a socket a local reverse proxy
        /// connects to. A browser needs the host:port form
        #[arg(short, long, env = "REMOTEX_LISTEN")]
        listen: Option<String>,
    },

    /// Start the gateway inside remotex.app: an ephemeral port on 127.0.0.1, the
    /// SPA out of the app's bundle, and a token printed on stdout for the app
    /// that started it.
    ///
    /// Not for interactive use. It serves the one client it was started by, reads
    /// only <instance-dir>/remotex.toml, and stops when its stdin closes — which
    /// is how it dies with the app.
    ServeEmbedded {
        /// The app's instance directory, holding remotex.toml, gateway.log and
        /// the viewer's own preferences. Nothing outside it is read.
        #[arg(long)]
        instance_dir: PathBuf,
        /// Where the built SPA lives — Contents/Resources/web inside the app
        /// bundle. Passed rather than derived: nothing about a bundle's layout
        /// is this binary's to know.
        #[arg(long)]
        web_root: PathBuf,
    },

    /// Check a config file and say what is wrong with it, without starting
    /// anything. Reads stdin unless --config names a file.
    ///
    /// This is what remotex.app's configuration editor calls before it saves, so
    /// that what the editor accepts is what the gateway accepts.
    CheckConfig {
        /// The file to check (default: read the config from stdin)
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Apply remotex.app's rules instead of a served gateway's: no [server]
        /// block, and no targets configured yet is not an error
        #[arg(long)]
        embedded: bool,
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
    fn serve_parses_config() {
        let cli = Cli::try_parse_from(["remotex", "serve", "-c", "/etc/x.toml"]).unwrap();
        let Commands::Serve { config, .. } = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(config.as_deref(), Some(std::path::Path::new("/etc/x.toml")));
    }

    #[test]
    fn serve_config_is_optional() {
        let cli = Cli::try_parse_from(["remotex", "serve"]).unwrap();
        let Commands::Serve { config, listen } = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert!(config.is_none());
        assert!(listen.is_none(), "unset means the config file decides");
    }

    /// The listen address is one value on the command line as it is one key in
    /// the file: there is no `--host`/`--port` pair to disagree with each other.
    #[test]
    fn serve_takes_one_listen_address() {
        for argv in [
            vec!["remotex", "serve", "--listen", "0.0.0.0:8080"],
            vec!["remotex", "serve", "-l", "0.0.0.0:8080"],
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap();
            let Commands::Serve { listen, .. } = cli.command else {
                panic!("expected the serve subcommand");
            };
            assert_eq!(listen.as_deref(), Some("0.0.0.0:8080"));
        }
        for gone in ["--host", "--port"] {
            assert!(
                Cli::try_parse_from(["remotex", "serve", gone, "x"]).is_err(),
                "{gone} is half an address: the listen address is one option"
            );
        }
    }

    #[test]
    fn serve_rejects_the_removed_target_selector() {
        // Target selection is browser-side now: --target is gone.
        assert!(Cli::try_parse_from(["remotex", "serve", "--target", "win"]).is_err());
    }

    /// The app's own subcommand. It takes the two paths only the app knows — the
    /// instance it owns and where its bundle keeps the SPA — and nothing else: the
    /// port and the secret are the gateway's to decide, not the app's to pass.
    #[test]
    fn serve_embedded_takes_the_two_paths_the_app_knows() {
        let cli = Cli::try_parse_from([
            "remotex",
            "serve-embedded",
            "--instance-dir",
            "/i",
            "--web-root",
            "/w",
        ])
        .unwrap();
        let Commands::ServeEmbedded {
            instance_dir,
            web_root,
        } = cli.command
        else {
            panic!("expected the serve-embedded subcommand");
        };
        assert_eq!(instance_dir, std::path::Path::new("/i"));
        assert_eq!(web_root, std::path::Path::new("/w"));

        for missing in [
            vec!["remotex", "serve-embedded"],
            vec!["remotex", "serve-embedded", "--instance-dir", "/i"],
            vec!["remotex", "serve-embedded", "--web-root", "/w"],
        ] {
            assert!(
                Cli::try_parse_from(&missing).is_err(),
                "neither path has a default: the app names both {missing:?}"
            );
        }
        for rejected in ["--port", "--gateway", "--token"] {
            assert!(
                Cli::try_parse_from([
                    "remotex",
                    "serve-embedded",
                    "--instance-dir",
                    "/i",
                    "--web-root",
                    "/w",
                    rejected,
                    "x"
                ])
                .is_err(),
                "{rejected} is not the app's to pass"
            );
        }
    }

    #[test]
    fn check_config_reads_stdin_by_default_and_takes_an_audience() {
        let cli = Cli::try_parse_from(["remotex", "check-config"]).unwrap();
        let Commands::CheckConfig { config, embedded } = cli.command else {
            panic!("expected the check-config subcommand");
        };
        assert!(config.is_none(), "stdin, which is what an unsaved editor has");
        assert!(!embedded, "a served gateway's rules unless asked otherwise");

        let cli =
            Cli::try_parse_from(["remotex", "check-config", "--embedded", "-c", "/i/remotex.toml"])
                .unwrap();
        let Commands::CheckConfig { config, embedded } = cli.command else {
            panic!("expected the check-config subcommand");
        };
        assert_eq!(config.as_deref(), Some(std::path::Path::new("/i/remotex.toml")));
        assert!(embedded);
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

}
