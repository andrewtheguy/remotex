//! TOML configuration: a `[server]` block plus `[[targets]]` profiles (see
//! docs/architecture.md).
//!
//! Config comes **only** from the TOML file (plus the `--config` selector).
//! There are deliberately no environment variables and no `.env` loading — env
//! files shadowing the real environment caused subtle setup bugs, and
//! credentials belong in one 600-mode file. The target is not selected on the
//! command line either: the server serves *every* `[[targets]]` profile and the
//! browser picks one after login (the post-login target picker), so there is a
//! single pathway to choosing a target.
//!
//! The config is **global-only**: the installed `<prefix>/etc/remotex.toml`, or
//! an explicit `--config <path>`. No per-user or working-directory files are
//! searched — one deployment, one config, no shadowing.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

use crate::auth::SitePasswd;

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
/// the same browser protocol (docs/architecture.md): `rdp` via IronRDP
/// (src/rdp.rs), `vnc` via the built-in RFB client (src/vnc.rs), `rxa` via the
/// purpose-built macOS agent (src/rxa.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Rdp,
    Vnc,
    Rxa,
}

impl Protocol {
    /// The protocol's standard port, used when a target omits `port`.
    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Rdp => 3389,
            Protocol::Vnc => 5900,
            // Adjacent to the web server's 52380, in the same private range,
            // colliding with neither 3389 nor 5900.
            Protocol::Rxa => rxa_proto::DEFAULT_PORT,
        }
    }

    /// The lowercase name, as written in the config file.
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Rdp => "rdp",
            Protocol::Vnc => "vnc",
            Protocol::Rxa => "rxa",
        }
    }
}

/// Operating system running on the remote target.
///
/// This is deliberately independent from [`Protocol`]: VNC and RDP can both
/// front more than one guest OS, while the native viewer needs the guest's
/// shortcut convention to decide whether local Command shortcuts should become
/// remote Control shortcuts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuestOs {
    Windows,
    Macos,
    Linux,
}

impl GuestOs {
    /// The lowercase name used by the wire protocol and frontend.
    pub fn name(self) -> &'static str {
        match self {
            GuestOs::Windows => "windows",
            GuestOs::Macos => "macos",
            GuestOs::Linux => "linux",
        }
    }
}

/// One `[[targets]]` profile: a remote machine plus its credentials.
///
/// Credentials live here (server-side) and are used during the RDP handshake.
/// They are never sent to the browser — see docs/architecture.md.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// Unique profile name; shown in the post-login target picker and selected
    /// there by the browser.
    pub name: String,
    /// Remote-desktop protocol: `"rdp"`, `"vnc"` or `"rxa"`. Required — each
    /// target must say what it speaks.
    pub protocol: Protocol,
    /// Remote operating system: `"windows"`, `"macos"`, or `"linux"`.
    /// Required because the transport protocol does not reliably identify the
    /// guest and native shortcut translation must remain backend-agnostic.
    pub os: GuestOs,
    /// Target host.
    pub host: String,
    /// Target port. Omitted (or 0) means the protocol's standard port
    /// (3389 for RDP, 5900 for VNC, 52381 for rxa) — normalized in
    /// [`ConfigFile::parse`].
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
    /// Dynamic resize: drive the remote desktop size from the browser viewport.
    /// VNC (`SetDesktopSize`, TigerVNC-family servers) resizes automatically as
    /// the viewport changes. RDP negotiates the Display Control channel and
    /// resizes only when the user asks (the floating menu's "Resize to window"),
    /// since RDP's Deactivation-Reactivation is heavier than VNC's resize. Off
    /// by default; without it (or on servers that can't resize) the desktop
    /// keeps its connect-time size and the browser shows scrollbars.
    #[serde(default)]
    pub resize: bool,
    /// Clipboard bridge: let the browser read and write this target's
    /// clipboard, through the floating menu's Clipboard panel. Off by default —
    /// a remote desktop's clipboard often holds whatever was last copied there,
    /// so exposing it is a per-target decision rather than a default.
    ///
    /// Supported by every engine, though what reaches the far side differs:
    /// `vnc` uses RFB `ServerCutText`/`ClientCutText` and is latin-1, so
    /// anything outside it becomes `?`; `rdp` uses the MS-RDPECLIP virtual
    /// channel with `CF_UNICODETEXT`; `rxa` has the Mac agent read and write
    /// `NSPasteboard`. The latter two are UTF-8 end to end.
    #[serde(default)]
    pub clipboard: bool,
    /// Pre-shared key for an `rxa` target, matching the `psk` in the Mac
    /// agent's own config file. This is the entire credential: the Noise
    /// handshake authenticates both sides from it, so a reconnect never
    /// involves a person. Required for `rxa`, rejected for anything else —
    /// see [`ConfigFile::parse`].
    ///
    /// `#[serde(default)]` because [`TargetConfig`] is one struct for every
    /// protocol and `deny_unknown_fields` leaves no room for a per-protocol
    /// shape — the same arrangement as the RDP-only `security`/`width`/`height`.
    #[serde(default)]
    pub psk: String,
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
    /// Web-login credential: `username:bcrypt_hash`, generated with
    /// `remotex gen-passwd <username>`. Required — without a login everything
    /// but the SPA shell and `/api/auth/*` refuses requests, so an empty
    /// value would lock the server to nobody.
    pub site_passwd: Option<String>,
    /// Display name shown on the login screen, the interstitials, and as the
    /// browser tab title. Defaults to [`DEFAULT_BRANDING`]; whitespace-only is
    /// treated as absent.
    pub branding: Option<String>,
}

/// The default display name when `[server].branding` is unset.
pub const DEFAULT_BRANDING: &str = "remotex";

/// The parsed TOML file, before a target is selected.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

/// Resolved runtime configuration: the web server plus every target profile it
/// serves (the browser picks one after login).
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Host/interface the web server binds to.
    pub host: String,
    /// Port the web server binds to.
    pub port: u16,
    /// Directory holding the built frontend (index.html + assets), served from
    /// disk. Defaults to [`default_static_dir`].
    pub static_dir: PathBuf,
    /// Every target profile this process serves; the post-login picker selects
    /// one. Guaranteed non-empty by [`ConfigFile::parse`].
    pub targets: Vec<TargetConfig>,
    /// Web-login credential guarding `/api/*` and `/ws`.
    pub site_passwd: SitePasswd,
    /// Display name for the login screen, interstitials, and browser tab title.
    pub branding: String,
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
        // The PSK is validated here rather than at connect time: a mistyped key
        // should fail on startup with the CRC's "transcription typo" hint, not
        // as a handshake rejection the first time someone picks the target.
        for target in &config.targets {
            if target.protocol == Protocol::Rxa {
                anyhow::ensure!(
                    target.os == GuestOs::Macos,
                    "target {:?} uses protocol \"rxa\", so os must be \"macos\"",
                    target.name
                );
                let psk = target.psk.trim();
                anyhow::ensure!(
                    !psk.is_empty(),
                    "target {:?} is protocol \"rxa\" but has no psk — \
                     generate one with `remotex gen-psk`",
                    target.name
                );
                rxa_proto::psk::parse(psk)
                    .map_err(|e| anyhow::anyhow!("target {:?} has an invalid psk: {e}", target.name))?;
                // `resize` is accepted here but means something narrower than it
                // does for RDP/VNC, and the agent — not this file — is what
                // enforces it: the browser gets a menu of the resolutions the
                // Mac's display advertises, and only when that display is a
                // virtual one. On a physical Mac the agent reports itself
                // unresizable and the menu never appears, because changing a
                // real panel's mode rearranges the screen of whoever is sitting
                // at it.
            } else {
                anyhow::ensure!(
                    target.psk.is_empty(),
                    "target {:?} is protocol {:?} but sets psk, which only \"rxa\" targets use",
                    target.name,
                    target.protocol.name()
                );
            }
        }
        Ok(config)
    }

    /// Resolve the runtime configuration: validate the web-login credential and
    /// carry over every target profile (the browser picks one after login).
    pub fn resolve(self) -> anyhow::Result<AppConfig> {
        let site_passwd = self
            .server
            .site_passwd
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context(
                "[server].site_passwd is required — generate one with \
                 `remotex gen-passwd <username>`",
            )?;
        let site_passwd =
            SitePasswd::parse(site_passwd).context("invalid [server].site_passwd")?;
        Ok(AppConfig {
            host: self.server.host.unwrap_or_else(|| "127.0.0.1".to_owned()),
            port: self.server.port.unwrap_or(52380),
            static_dir: self.server.static_dir.unwrap_or_else(default_static_dir),
            // Non-empty is guaranteed by `parse`.
            targets: self.targets,
            site_passwd,
            branding: self
                .server
                .branding
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_BRANDING)
                .to_owned(),
        })
    }
}

/// Load the config file: the explicit `--config` path, or the global
/// `<prefix>/etc/remotex.toml` of the installed layout. Returns the parsed file
/// and the path it came from.
pub fn load(explicit: Option<&Path>) -> anyhow::Result<(ConfigFile, PathBuf)> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => installed_config_path().context(
            "no --config given and not running from an installed prefix \
             (<prefix>/versions/<version>/bin/remotex) — pass --config <path>",
        )?,
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config =
        ConfigFile::parse(&text).with_context(|| format!("in config file {}", path.display()))?;
    Ok((config, path))
}

/// The one global config location, `<prefix>/etc/remotex.toml`, when the
/// executable runs from the versioned install layout (see packaging/README.md).
pub fn installed_config_path() -> Option<PathBuf> {
    Some(installed_etc_dir()?.join("remotex.toml"))
}

/// The active version root, derived from the running binary's own location.
///
/// The binary is shipped at `<prefix>/versions/<version>/bin/remotex`. We
/// canonicalize `current_exe` so a launcher symlink resolves to the real
/// versioned directory. Returns `None` in odd environments where the executable
/// path can't be determined.
pub fn version_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    // <root>/bin/remotex → <root>
    Some(exe.parent()?.parent()?.to_path_buf())
}

/// `<prefix>/etc` when the executable lives in the versioned install layout
/// (`<prefix>/versions/<version>/bin/remotex`), else `None`.
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
/// Prefers the installed layout (`<root>/share/remotex/web`); falls back to
/// `frontend/dist` relative to the working directory for `cargo run` in a
/// checkout. Override with `static_dir` in the `[server]` block.
pub fn default_static_dir() -> PathBuf {
    if let Some(root) = version_root() {
        let installed = root.join("share/remotex/web");
        if installed.is_dir() {
            return installed;
        }
    }
    PathBuf::from("frontend/dist")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid `site_passwd = "…"` line (admin/hunter2 at bcrypt's minimum
    /// cost) for configs that get resolved — resolve requires the credential.
    fn site_passwd_line() -> String {
        let encoded = crate::auth::generate("admin", "hunter2", 4).unwrap();
        format!("site_passwd = \"{encoded}\"")
    }

    fn minimal() -> String {
        format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "one"
            protocol = "rdp"
            os = "windows"
            host = "192.0.2.10"
            "#,
            site_passwd_line()
        )
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let config = ConfigFile::parse(&minimal()).unwrap().resolve().unwrap();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 52380);
        assert_eq!(config.site_passwd.username(), "admin");
        assert_eq!(config.targets.len(), 1);
        let t = &config.targets[0];
        assert_eq!(t.name, "one");
        assert_eq!(t.protocol, Protocol::Rdp);
        assert_eq!(t.os, GuestOs::Windows);
        assert_eq!((t.host.as_str(), t.port), ("192.0.2.10", 3389));
        assert_eq!((t.width, t.height), (1280, 800));
        assert_eq!(t.security, Security::Auto);
        assert!(t.username.is_empty() && t.password.is_empty() && t.domain.is_none());
        assert!(!t.resize, "dynamic resize is opt-in");
        assert!(!t.clipboard, "the clipboard bridge is opt-in");
    }

    #[test]
    fn branding_defaults_and_overrides() {
        // Unset → the default name.
        let config = ConfigFile::parse(&minimal()).unwrap().resolve().unwrap();
        assert_eq!(config.branding, DEFAULT_BRANDING);

        // Set → carried through, trimmed.
        let toml = format!(
            r#"
            [server]
            branding = "  Acme Remote  "
            {}

            [[targets]]
            name = "one"
            protocol = "rdp"
            os = "windows"
            host = "192.0.2.10"
            "#,
            site_passwd_line()
        );
        let config = ConfigFile::parse(&toml).unwrap().resolve().unwrap();
        assert_eq!(config.branding, "Acme Remote");

        // Whitespace-only → falls back to the default.
        let toml = toml.replace("  Acme Remote  ", "   ");
        let config = ConfigFile::parse(&toml).unwrap().resolve().unwrap();
        assert_eq!(config.branding, DEFAULT_BRANDING);
    }

    #[test]
    fn full_config_parses() {
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            host = "0.0.0.0"
            port = 8080
            static_dir = "/srv/web"
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            os = "windows"
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
            os = "linux"
            host = "10.0.0.3"
            "#,
            site_passwd_line()
        ))
        .unwrap();
        let config = config.resolve().unwrap();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 8080);
        assert_eq!(config.static_dir, PathBuf::from("/srv/web"));
        // Every profile is carried over, in file order, for the picker.
        assert_eq!(config.targets.len(), 2);
        let win = &config.targets[0];
        assert_eq!(win.name, "win");
        assert_eq!(win.security, Security::Nla);
        assert_eq!(win.domain.as_deref(), Some("CORP"));
        assert_eq!((win.width, win.height), (1920, 1080));
        let other = &config.targets[1];
        assert_eq!(other.name, "other");
        assert_eq!(other.protocol, Protocol::Vnc);
    }

    #[test]
    fn missing_site_passwd_is_rejected() {
        // Parse succeeds (the file is well-formed); resolve refuses to run
        // without the web-login credential and says how to make one.
        let toml = r#"
            [[targets]]
            name = "one"
            protocol = "rdp"
            os = "windows"
            host = "192.0.2.10"
        "#;
        let err = ConfigFile::parse(toml).unwrap().resolve().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("site_passwd") && msg.contains("gen-passwd"), "{msg}");

        // Whitespace-only is as good as absent.
        let toml = format!("[server]\nsite_passwd = \"  \"\n{toml}");
        let err = ConfigFile::parse(&toml).unwrap().resolve().unwrap_err();
        assert!(format!("{err:#}").contains("site_passwd"), "{err:#}");
    }

    #[test]
    fn malformed_site_passwd_is_rejected() {
        let toml = r#"
            [server]
            site_passwd = "no-colon-in-here"

            [[targets]]
            name = "one"
            protocol = "rdp"
            os = "windows"
            host = "192.0.2.10"
        "#;
        let err = ConfigFile::parse(toml).unwrap().resolve().unwrap_err();
        assert!(format!("{err:#}").contains("username:bcrypt_hash"), "{err:#}");
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
            os = "windows"
            host = "h1"
            [[targets]]
            name = "a"
            protocol = "rdp"
            os = "windows"
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
            os = "windows"
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
            os = "windows"
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
            os = "linux"
            "#,
        )
        .unwrap_err();
        // The error should say what is supported.
        let msg = format!("{err:#}");
        assert!(msg.contains("rdp") && msg.contains("vnc"), "{msg}");
    }

    #[test]
    fn missing_or_unknown_guest_os_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "vnc"
            host = "h"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("os"), "{err:#}");

        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "vnc"
            os = "android"
            host = "h"
            "#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("windows") && msg.contains("macos") && msg.contains("linux"),
            "{msg}"
        );
    }

    #[test]
    fn rxa_target_requires_macos_guest_os() {
        let psk = rxa_proto::psk::generate();
        let toml = rxa_toml(&format!("psk = \"{psk}\""))
            .replace("os = \"macos\"", "os = \"windows\"");
        let err = ConfigFile::parse(&toml).unwrap_err();
        assert!(format!("{err:#}").contains("macos"), "{err:#}");
    }

    #[test]
    fn vnc_target_gets_the_vnc_default_port() {
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "mac"
            protocol = "vnc"
            os = "macos"
            host = "10.0.0.4"
            password = "hunter2"
            resize = true
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(config.targets[0].protocol, Protocol::Vnc);
        assert_eq!(config.targets[0].port, 5900);
        assert!(config.targets[0].resize);

        // An explicit port wins over the protocol default.
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "mac"
            protocol = "vnc"
            os = "macos"
            host = "10.0.0.4"
            port = 5901
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(config.targets[0].port, 5901);
    }

    /// An `rxa` target body, with `psk` filled in from a freshly generated key
    /// unless `psk` is given.
    fn rxa_toml(extra: &str) -> String {
        format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "mac"
            protocol = "rxa"
            os = "macos"
            host = "mac.local"
            {extra}
            "#,
            site_passwd_line()
        )
    }

    #[test]
    fn rxa_target_parses_with_its_own_default_port() {
        let psk = rxa_proto::psk::generate();
        let config = ConfigFile::parse(&rxa_toml(&format!("psk = \"{psk}\"")))
            .unwrap()
            .resolve()
            .unwrap();
        let target = &config.targets[0];
        assert_eq!(target.protocol, Protocol::Rxa);
        assert_eq!(target.protocol.name(), "rxa");
        // Adjacent to the web server's 52380, and not 3389 or 5900.
        assert_eq!(target.port, 52381);
        assert_eq!(target.psk, psk);
        assert!(!target.resize, "resize is opt-in for rxa too");

        // An explicit port still wins.
        let config = ConfigFile::parse(&rxa_toml(&format!("psk = \"{psk}\"\nport = 52999")))
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(config.targets[0].port, 52999);
    }

    // A mistyped PSK must fail on startup with the CRC's hint, not as an opaque
    // handshake rejection the first time someone picks the target.
    #[test]
    fn rxa_psk_is_validated_at_parse_time() {
        let err = ConfigFile::parse(&rxa_toml("")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no psk") && msg.contains("gen-psk"), "{msg}");

        let err = ConfigFile::parse(&rxa_toml("psk = \"rxanope\"")).unwrap_err();
        assert!(format!("{err:#}").contains("invalid psk"), "{err:#}");

        // A single-character typo in an otherwise well-formed key.
        let psk = rxa_proto::psk::generate();
        let mut chars: Vec<char> = psk.chars().collect();
        chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        let err = ConfigFile::parse(&rxa_toml(&format!("psk = \"{typo}\""))).unwrap_err();
        assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    }

    #[test]
    fn a_psk_on_a_non_rxa_target_is_rejected() {
        // Silently ignoring it would leave someone believing a VNC target was
        // authenticated by a key it never uses.
        let psk = rxa_proto::psk::generate();
        let err = ConfigFile::parse(&format!(
            r#"
            [[targets]]
            name = "one"
            protocol = "vnc"
            os = "linux"
            host = "h"
            psk = "{psk}"
            "#
        ))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("psk") && msg.contains("rxa"), "{msg}");
    }

    // Whether a resize can actually happen is the agent's call (a virtual
    // display, in a VM), so the config layer takes the opt-in at face value
    // rather than second-guessing a Mac it cannot see.
    #[test]
    fn resize_is_accepted_on_an_rxa_target() {
        let psk = rxa_proto::psk::generate();
        let config = ConfigFile::parse(&rxa_toml(&format!("psk = \"{psk}\"\nresize = true")))
            .unwrap()
            .resolve()
            .unwrap();
        assert!(config.targets[0].resize);
    }

    #[test]
    fn clipboard_is_accepted_for_every_protocol() {
        let psk = rxa_proto::psk::generate();
        let config = ConfigFile::parse(&rxa_toml(&format!("psk = \"{psk}\"\nclipboard = true")))
            .unwrap()
            .resolve()
            .unwrap();
        assert!(config.targets[0].clipboard);

        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "box"
            protocol = "vnc"
            os = "linux"
            host = "10.0.0.4"
            clipboard = true
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].clipboard);

        // RDP was the last engine to gain a clipboard (MS-RDPECLIP) and used to
        // be refused here; the flag is now accepted for all three.
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            os = "windows"
            host = "10.0.0.5"
            clipboard = true
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].clipboard);
    }
}
