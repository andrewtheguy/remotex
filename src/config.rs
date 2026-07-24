//! TOML configuration, in remotex's shape: a `[server]` block plus
//! `[[targets]]` profiles (see docs/phase2-consolidation.md).
//!
//! Config comes **only** from the TOML file (plus the `--config`/`--target`
//! CLI selectors). There are deliberately no environment variables and no
//! `.env` loading — env files shadowing the real environment caused subtle
//! bugs in the remotex/bun setup, and credentials belong in one 600-mode file.
//!
//! The config is **global-only**: the installed `<prefix>/etc/rdpweb.toml`, or
//! an explicit `--config <path>`. No per-user or working-directory files are
//! searched — one deployment, one config, no shadowing.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

/// RDP security negotiation mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// Advertise both TLS and NLA/CredSSP; the server picks the strongest.
    #[default]
    Auto,
    /// Require NLA/CredSSP (network-level auth before the session).
    Nla,
    /// Plain TLS security only — no NLA; the remote shows a graphical login.
    Tls,
}

impl Security {
    /// `(enable_tls, enable_credssp)` for the IronRDP connector config.
    pub fn flags(self) -> (bool, bool) {
        match self {
            Security::Auto => (true, true),
            Security::Nla => (false, true),
            Security::Tls => (true, false),
        }
    }
}

/// Remote-desktop protocol of a target. Each has a server-side engine feeding
/// the same browser protocol (docs/phase2-consolidation.md): `rdp` via IronRDP
/// (src/rdp.rs), `vnc` via the built-in RFB client (src/vnc.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Rdp,
    Vnc,
}

impl Protocol {
    /// The protocol's standard port, used when a target omits `port`.
    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Rdp => 3389,
            Protocol::Vnc => 5900,
        }
    }

    /// The lowercase name, as written in the config file.
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Rdp => "rdp",
            Protocol::Vnc => "vnc",
        }
    }
}

/// One `[[targets]]` profile: a remote machine plus its credentials.
///
/// Credentials live here (server-side) and are used during the RDP handshake.
/// They are never sent to the browser — see docs/phase1-mvp.md.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// Unique profile name; selected with `--target` (default: first profile).
    pub name: String,
    /// Remote-desktop protocol: `"rdp"` or `"vnc"`. Required — each target
    /// must say what it speaks.
    pub protocol: Protocol,
    /// Target host.
    pub host: String,
    /// Target port. Omitted (or 0) means the protocol's standard port
    /// (3389 for RDP, 5900 for VNC) — normalized in [`ConfigFile::parse`].
    #[serde(default)]
    pub port: u16,
    /// Username.
    #[serde(default)]
    pub username: String,
    /// Password (never leaves the server).
    #[serde(default)]
    pub password: String,
    /// Optional domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// Initial desktop width requested from the server.
    #[serde(default = "default_width")]
    pub width: u16,
    /// Initial desktop height requested from the server.
    #[serde(default = "default_height")]
    pub height: u16,
    /// Security negotiation mode: `"auto"`, `"nla"`, or `"tls"`. RDP only —
    /// ignored for VNC targets (RFB security is negotiated per the handshake).
    #[serde(default)]
    pub security: Security,
}

fn default_width() -> u16 {
    1280
}
fn default_height() -> u16 {
    800
}

/// The optional `[server]` block: web-server bind and frontend location.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    /// Host/interface the web server binds to (default `127.0.0.1`).
    pub host: Option<String>,
    /// Port the web server binds to (default `52380`).
    pub port: Option<u16>,
    /// Directory holding the built frontend; overrides [`default_static_dir`].
    pub static_dir: Option<PathBuf>,
}

/// The parsed TOML file, before a target is selected.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

/// Resolved runtime configuration: the web server plus the one selected target.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Host/interface the web server binds to.
    pub host: String,
    /// Port the web server binds to.
    pub port: u16,
    /// Directory holding the built frontend (index.html + assets), served from
    /// disk. Defaults to [`default_static_dir`].
    pub static_dir: PathBuf,
    /// The target profile this process serves.
    pub target: TargetConfig,
}

impl ConfigFile {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let mut config: ConfigFile = toml::from_str(text).context("invalid TOML config")?;
        // An omitted port deserializes as 0 (never a valid target port), which
        // resolves here to the protocol's standard port.
        for target in &mut config.targets {
            if target.port == 0 {
                target.port = target.protocol.default_port();
            }
        }
        anyhow::ensure!(
            !config.targets.is_empty(),
            "config has no [[targets]] — at least one target profile is required"
        );
        for target in &config.targets {
            anyhow::ensure!(
                !target.name.is_empty(),
                "a [[targets]] entry has an empty name"
            );
            anyhow::ensure!(
                !target.host.is_empty(),
                "target {:?} has an empty host",
                target.name
            );
        }
        for (i, target) in config.targets.iter().enumerate() {
            anyhow::ensure!(
                !config.targets[..i].iter().any(|t| t.name == target.name),
                "duplicate target name {:?}",
                target.name
            );
        }
        Ok(config)
    }

    /// Pick the target profile to serve: by name, or the first one.
    pub fn resolve(self, target_name: Option<&str>) -> anyhow::Result<AppConfig> {
        let target = match target_name {
            Some(name) => self
                .targets
                .iter()
                .find(|t| t.name == name)
                .cloned()
                .with_context(|| {
                    format!(
                        "no target named {name:?} (available: {})",
                        self.targets
                            .iter()
                            .map(|t| t.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?,
            // Non-empty is guaranteed by `parse`.
            None => self.targets.into_iter().next().context("no targets")?,
        };
        Ok(AppConfig {
            host: self.server.host.unwrap_or_else(|| "127.0.0.1".to_owned()),
            port: self.server.port.unwrap_or(52380),
            static_dir: self.server.static_dir.unwrap_or_else(default_static_dir),
            target,
        })
    }
}

/// Load the config file: the explicit `--config` path, or the global
/// `<prefix>/etc/rdpweb.toml` of the installed layout. Returns the parsed file
/// and the path it came from.
pub fn load(explicit: Option<&Path>) -> anyhow::Result<(ConfigFile, PathBuf)> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => installed_config_path().context(
            "no --config given and not running from an installed prefix \
             (<prefix>/versions/<version>/bin/rdpweb) — pass --config <path>",
        )?,
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config =
        ConfigFile::parse(&text).with_context(|| format!("in config file {}", path.display()))?;
    Ok((config, path))
}

/// The one global config location, `<prefix>/etc/rdpweb.toml`, when the
/// executable runs from the versioned install layout (see packaging/README.md).
pub fn installed_config_path() -> Option<PathBuf> {
    Some(installed_etc_dir()?.join("rdpweb.toml"))
}

/// The active version root, derived from the running binary's own location.
///
/// The binary is shipped at `<prefix>/versions/<version>/bin/rdpweb`. We
/// canonicalize `current_exe` so a launcher symlink resolves to the real
/// versioned directory. Returns `None` in odd environments where the executable
/// path can't be determined.
pub fn version_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    // <root>/bin/rdpweb → <root>
    Some(exe.parent()?.parent()?.to_path_buf())
}

/// `<prefix>/etc` when the executable lives in the versioned install layout
/// (`<prefix>/versions/<version>/bin/rdpweb`), else `None`.
fn installed_etc_dir() -> Option<PathBuf> {
    let root = version_root()?;
    let versions_dir = root.parent()?;
    if versions_dir.file_name()? != "versions" {
        return None;
    }
    Some(versions_dir.parent()?.join("etc"))
}

/// Default location of the built frontend.
///
/// Prefers the installed layout (`<root>/share/rdpweb/web`); falls back to
/// `frontend/dist` relative to the working directory for `cargo run` in a
/// checkout. Override with `static_dir` in the `[server]` block.
pub fn default_static_dir() -> PathBuf {
    if let Some(root) = version_root() {
        let installed = root.join("share/rdpweb/web");
        if installed.is_dir() {
            return installed;
        }
    }
    PathBuf::from("frontend/dist")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [[targets]]
        name = "one"
        protocol = "rdp"
        host = "192.0.2.10"
    "#;

    #[test]
    fn minimal_config_gets_defaults() {
        let config = ConfigFile::parse(MINIMAL).unwrap().resolve(None).unwrap();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 52380);
        let t = &config.target;
        assert_eq!(t.name, "one");
        assert_eq!(t.protocol, Protocol::Rdp);
        assert_eq!((t.host.as_str(), t.port), ("192.0.2.10", 3389));
        assert_eq!((t.width, t.height), (1280, 800));
        assert_eq!(t.security, Security::Auto);
        assert!(t.username.is_empty() && t.password.is_empty() && t.domain.is_none());
    }

    #[test]
    fn full_config_parses() {
        let config = ConfigFile::parse(
            r#"
            [server]
            host = "0.0.0.0"
            port = 8080
            static_dir = "/srv/web"

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "10.0.0.2"
            port = 3390
            username = "Administrator"
            password = "hunter2"
            domain = "CORP"
            width = 1920
            height = 1080
            security = "nla"

            [[targets]]
            name = "other"
            protocol = "vnc"
            host = "10.0.0.3"
            "#,
        )
        .unwrap();
        let config = config.resolve(Some("win")).unwrap();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 8080);
        assert_eq!(config.static_dir, PathBuf::from("/srv/web"));
        let t = &config.target;
        assert_eq!(t.security, Security::Nla);
        assert_eq!(t.domain.as_deref(), Some("CORP"));
        assert_eq!((t.width, t.height), (1920, 1080));
    }

    #[test]
    fn unknown_target_name_lists_available() {
        let err = ConfigFile::parse(MINIMAL)
            .unwrap()
            .resolve(Some("nope"))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nope") && msg.contains("one"), "{msg}");
    }

    #[test]
    fn no_targets_is_rejected() {
        assert!(ConfigFile::parse("[server]\nport = 1").is_err());
    }

    #[test]
    fn duplicate_target_names_are_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h1"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h2"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("duplicate"), "{err:#}");
    }

    #[test]
    fn typos_are_rejected() {
        // deny_unknown_fields: a misspelled key is an error, not silence.
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            passwd = "oops"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("passwd"), "{err:#}");

        // Same for the [server] block and the top level.
        let err = ConfigFile::parse("[server]\nprot = 1").unwrap_err();
        assert!(format!("{err:#}").contains("prot"), "{err:#}");
        let err = ConfigFile::parse("[srv]\nport = 1").unwrap_err();
        assert!(format!("{err:#}").contains("srv"), "{err:#}");
    }

    #[test]
    fn missing_protocol_is_rejected() {
        // No default protocol: every target must say what it speaks.
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            host = "h"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("protocol"), "{err:#}");
    }

    #[test]
    fn unknown_protocol_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            host = "h"
            protocol = "telnet"
            "#,
        )
        .unwrap_err();
        // The error should say what is supported.
        let msg = format!("{err:#}");
        assert!(msg.contains("rdp") && msg.contains("vnc"), "{msg}");
    }

    #[test]
    fn vnc_target_gets_the_vnc_default_port() {
        let config = ConfigFile::parse(
            r#"
            [[targets]]
            name = "mac"
            protocol = "vnc"
            host = "10.0.0.4"
            password = "hunter2"
            "#,
        )
        .unwrap()
        .resolve(None)
        .unwrap();
        assert_eq!(config.target.protocol, Protocol::Vnc);
        assert_eq!(config.target.port, 5900);

        // An explicit port wins over the protocol default.
        let config = ConfigFile::parse(
            r#"
            [[targets]]
            name = "mac"
            protocol = "vnc"
            host = "10.0.0.4"
            port = 5901
            "#,
        )
        .unwrap()
        .resolve(None)
        .unwrap();
        assert_eq!(config.target.port, 5901);
    }
}
