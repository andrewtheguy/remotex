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

/// A variant of a target's [`Protocol`]: same engine, different dialect at the
/// far end, and different rules about what a target may say.
///
/// Generic by design — a protocol with more than one flavour of server names
/// which one it is talking to here, rather than each protocol growing a key of
/// its own. Which subtypes a protocol accepts is [`ConfigFile::parse`]'s
/// business; today `ard` is the only one and only `vnc` takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Subtype {
    /// macOS Screen Sharing, authenticated the way Apple Remote Desktop does:
    /// the credentials are a *macOS account's* and the connection is named to
    /// the Mac, which is what makes it share the screen rather than a login
    /// window of its own (see [`crate::vnc`]). A third-party VNC server that
    /// happens to run on a Mac is not this — it is a plain `vnc` target.
    Ard,
}

impl Subtype {
    /// The lowercase name, as written in the config file.
    pub fn name(self) -> &'static str {
        match self {
            Subtype::Ard => "ard",
        }
    }
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
    /// Which flavour of [`Self::protocol`] the far end is, when the protocol has
    /// more than one: `subtype = "ard"` on a `vnc` target is macOS Screen
    /// Sharing. Unset means the protocol's ordinary form.
    ///
    /// Declared rather than sniffed from the credentials, because the two
    /// dialects want different ones and guessing which was meant is how a
    /// perfectly good password ends up authenticating nobody — see
    /// [`Subtype::Ard`]. Validated against the protocol in
    /// [`ConfigFile::parse`].
    #[serde(default)]
    pub subtype: Option<Subtype>,
    /// Target host.
    pub host: String,
    /// Target port. Omitted (or 0) means the protocol's standard port
    /// (3389 for RDP, 5900 for VNC, 52381 for rxa) — normalized in
    /// [`ConfigFile::parse`].
    #[serde(default)]
    pub port: u16,
    /// Username. Required by RDP, and by a `vnc` target of [`Subtype::Ard`],
    /// where it is a *macOS account* and [`Self::password`] is that account's —
    /// not the Screen Sharing password. A plain `vnc` target has no use for
    /// either and is refused both, because RFB `VncAuth` cannot carry a name.
    #[serde(default)]
    pub username: String,
    /// Password for [`Self::username`] (never leaves the server).
    #[serde(default)]
    pub password: String,
    /// A VNC server's own password — RFB `VncAuth`, which proves knowledge of a
    /// secret belonging to the *machine* and says nothing about who is
    /// connecting. Named apart from [`Self::password`] because on a Mac the two
    /// are different credentials that get you different screens: this is the
    /// Screen Sharing password, and it is answered with a login window of the
    /// connection's own (see [`crate::vnc`]).
    ///
    /// The credential of a plain `vnc` target, and the only one such a server
    /// takes. Rejected on other protocols and on [`Subtype::Ard`] — see
    /// [`ConfigFile::parse`].
    #[serde(default)]
    pub vnc_password: String,
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
                let psk = target.psk.trim();
                anyhow::ensure!(
                    !psk.is_empty(),
                    "target {:?} is protocol \"rxa\" but has no psk — \
                     generate one with `remotex gen-psk`",
                    target.name
                );
                rxa_proto::psk::parse(psk)
                    .map_err(|e| anyhow::anyhow!("target {:?} has an invalid psk: {e}", target.name))?;
                // Rejected rather than ignored. A Mac's resolution is the Mac's:
                // there is no message on the rxa wire that asks it to change,
                // and the agent's own display — the one it can create for itself
                // — appears in System Settings like any other screen and is
                // changed there. Accepting the key would promise a control that
                // neither client offers.
                anyhow::ensure!(
                    !target.resize,
                    "target {:?} is protocol \"rxa\" and sets resize, which only \"rdp\" and \
                     \"vnc\" targets use — a Mac's resolution is changed on the Mac",
                    target.name
                );
            } else {
                anyhow::ensure!(
                    target.psk.is_empty(),
                    "target {:?} is protocol {:?} but sets psk, which only \"rxa\" targets use",
                    target.name,
                    target.protocol.name()
                );
            }
            // Which credentials a VNC target may carry is the subtype's to say,
            // and the two sets do not overlap: `ard` authenticates an account to
            // a Mac, plain VncAuth proves a secret the machine holds. Mixing
            // them is how a password ends up authenticating nobody, so each is
            // refused where it cannot be used rather than quietly ignored.
            match (target.protocol, target.subtype) {
                (Protocol::Vnc, Some(Subtype::Ard)) => {
                    anyhow::ensure!(
                        !target.username.is_empty() && !target.password.is_empty(),
                        "target {:?} is subtype \"ard\" but has no username and password — \
                         both are needed, and on a Mac they are an account's there",
                        target.name
                    );
                    anyhow::ensure!(
                        target.vnc_password.is_empty(),
                        "target {:?} is subtype \"ard\" but sets vnc_password, which only a \
                         plain \"vnc\" target uses — Apple's authentication carries the \
                         account credentials above instead",
                        target.name
                    );
                    // Rejected for the same reason `rxa` rejects it: macOS
                    // accepts the resize negotiation and then ignores every
                    // request, so the key would promise a control that does
                    // nothing. A Mac's resolution is set on the Mac.
                    anyhow::ensure!(
                        !target.resize,
                        "target {:?} is subtype \"ard\" and sets resize, which macOS Screen \
                         Sharing accepts and then ignores — a Mac's resolution is changed on \
                         the Mac",
                        target.name
                    );
                }
                (Protocol::Vnc, None) => {
                    anyhow::ensure!(
                        target.username.is_empty() && target.password.is_empty(),
                        "target {:?} is protocol \"vnc\" and sets username or password, which \
                         plain VncAuth cannot carry — use vnc_password for the VNC server's \
                         own password, or subtype = \"ard\" if this is a Mac and those are an \
                         account's",
                        target.name
                    );
                }
                (protocol, Some(subtype)) => anyhow::bail!(
                    "target {:?} is protocol {:?} and sets subtype {:?}, which only \"vnc\" \
                     targets have",
                    target.name,
                    protocol.name(),
                    subtype.name()
                ),
                (_, None) => {}
            }
            if target.protocol != Protocol::Vnc {
                anyhow::ensure!(
                    target.vnc_password.is_empty(),
                    "target {:?} is protocol {:?} but sets vnc_password, which only \"vnc\" \
                     targets use",
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

    // Nothing in a target says what the remote runs. The engines discover it
    // (src/vnc.rs asks the RFB greeting; rxa is macOS by construction), so a
    // config that tried to declare it is a typo, not a supported knob.
    #[test]
    fn a_target_cannot_declare_the_remote_os() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "vnc"
            os = "windows"
            host = "h"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("os"), "{err:#}");
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
            host = "10.0.0.4"
            vnc_password = "hunter2"
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

    /// A `vnc` target body, with whatever keys the case is about.
    fn vnc_toml(extra: &str) -> String {
        format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "mac"
            protocol = "vnc"
            host = "10.0.0.4"
            {extra}
            "#,
            site_passwd_line()
        )
    }

    /// The two VNC credentials are different credentials, and the subtype — not
    /// which fields happen to be filled — says which one a target carries.
    /// Neither is silently ignored where it cannot be used: an account password
    /// answered as VncAuth authenticates nobody, which on a Mac shares a login
    /// window instead of the screen.
    #[test]
    fn each_vnc_credential_is_refused_where_it_cannot_be_used() {
        let err = ConfigFile::parse(&vnc_toml(r#"password = "hunter2""#)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("vnc_password") && msg.contains(r#"subtype = "ard""#), "{msg}");
        let err = ConfigFile::parse(&vnc_toml(r#"username = "andrew""#)).unwrap_err();
        assert!(format!("{err:#}").contains("plain VncAuth cannot carry"), "{err:#}");

        // A plain target takes the server's own password, and nothing at all is
        // still a target: a VNC server may need no credential whatsoever.
        let plain = ConfigFile::parse(&vnc_toml(r#"vnc_password = "hunter2""#)).unwrap();
        assert_eq!(plain.targets[0].vnc_password, "hunter2");
        assert!(plain.targets[0].subtype.is_none());
        assert!(ConfigFile::parse(&vnc_toml("")).is_ok());
    }

    /// `ard` is a declaration about the far end, so it comes with the credentials
    /// that declaration implies and refuses the ones it does not use.
    #[test]
    fn the_ard_subtype_wants_an_account_and_nothing_else() {
        let ard = |extra: &str| ConfigFile::parse(&vnc_toml(&format!("subtype = \"ard\"\n{extra}")));

        let target = &ard("username = \"andrew\"\npassword = \"hunter2\"")
            .unwrap()
            .targets[0];
        assert_eq!(target.subtype, Some(Subtype::Ard));
        assert_eq!(target.username, "andrew");

        // Half a credential is no credential.
        let err = ard(r#"username = "andrew""#).unwrap_err();
        assert!(format!("{err:#}").contains("no username and password"), "{err:#}");

        // The machine's own password has no part in it.
        let err = ard("username = \"andrew\"\npassword = \"h\"\nvnc_password = \"other\"")
            .unwrap_err();
        assert!(format!("{err:#}").contains("sets vnc_password"), "{err:#}");

        // Resize is rejected rather than ignored: macOS accepts the negotiation
        // and then does nothing with the requests.
        let err =
            ard("username = \"andrew\"\npassword = \"h\"\nresize = true").unwrap_err();
        assert!(format!("{err:#}").contains("accepts and then ignores"), "{err:#}");

        // And it is a VNC subtype only.
        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "pc"
            protocol = "rdp"
            host = "10.0.0.5"
            subtype = "ard"
            username = "Administrator"
            password = "hunter2"
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("only \"vnc\" targets have"), "{msg}");
    }

    #[test]
    fn a_vnc_password_on_a_non_vnc_target_is_rejected() {
        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "pc"
            protocol = "rdp"
            host = "10.0.0.5"
            vnc_password = "hunter2"
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("vnc_password") && msg.contains("vnc"), "{msg}");
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
            host = "h"
            psk = "{psk}"
            "#
        ))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("psk") && msg.contains("rxa"), "{msg}");
    }

    // Nothing in the rxa protocol carries a resize request, so the flag would be
    // a promise the wire cannot keep. Rejected on sight rather than ignored.
    #[test]
    fn resize_on_an_rxa_target_is_rejected() {
        let psk = rxa_proto::psk::generate();
        let err = ConfigFile::parse(&rxa_toml(&format!("psk = \"{psk}\"\nresize = true")))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("resize") && msg.contains("rxa"), "{msg}");
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
