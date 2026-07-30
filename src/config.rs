//! Global TOML configuration: one `[server]` block and `[[targets]]` profiles.
//! Only the selected config file is read; target credentials remain server-side.

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
    ///
    /// Read by RDP at connect, where it is genuinely the size asked for, and by
    /// both RDP and VNC as the answer to [`crate::protocol::ClientMsg::DefaultSize`]
    /// — a client with no desktop-shaped window of its own asking for whatever
    /// size this end considers right. A VNC server keeps its own size at connect
    /// and this is only ever consulted for a client that asks, so setting it costs
    /// a VNC target nothing and gives an operator somewhere to say what a phone
    /// should get. `rxa` ignores it: the size of a display the agent made is the
    /// agent's to state, in its own `virtual_display_initial_size`.
    ///
    /// On an RDP target with [`Self::resize`] this is a size in *points*: the
    /// connect happens at 1x, but a Retina client then asks for twice the pixels,
    /// and `DefaultSize` has to keep meaning the same desktop rather than half of
    /// one. See `Density` in src/rdp.rs.
    #[serde(default = "default_width")]
    pub width: u16,
    /// Initial desktop height requested from the server. See [`Self::width`].
    #[serde(default = "default_height")]
    pub height: u16,
    /// Security negotiation mode: `"auto"`, `"nla"`, or `"tls"`. RDP only —
    /// ignored for VNC targets (RFB security is negotiated per the handshake).
    #[serde(default)]
    pub security: Security,
    /// Allow client-driven resize: permission for a client to set this target's
    /// desktop size, and only permission — whether a client then follows its window
    /// continuously or resizes when asked is that client's own choice. RXA narrows
    /// where the permission reaches: an active agent-created display, never one of
    /// the Mac's own screens.
    ///
    /// On RDP this also turns on density matching, because there a density *is* a
    /// resize: the Display Control channel this negotiates is the only way to tell
    /// a live session to render at 200%, so a Retina client gets twice the pixels
    /// and a UI drawn twice as large. Off, an RDP target ignores the client's
    /// density entirely.
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
    /// Negotiate RDP audio at connect. Packets are sent only while the attached
    /// client subscribes. Rejected for VNC and RXA.
    #[serde(default)]
    pub audio: bool,
    /// Mac agent public key (`rxap…`). Required for RXA and rejected elsewhere.
    #[serde(default)]
    pub agent_public_key: String,
    /// Gateway private key copied from [`RxaSection`] during resolution.
    #[serde(skip)]
    pub gateway_private_key: String,
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
    /// **Development only.** A label to give this gateway its own hostname on
    /// loopback: a browser arriving at `127.0.0.1`, `::1` or `localhost` is
    /// redirected to `<label>.localhost`, keeping the port and path.
    ///
    /// It exists for one problem, which has no other clean answer: a cookie is
    /// scoped by *host* and ignores the port, so two gateways on one machine
    /// share `remotex_session` and each login silently evicts the other. The
    /// gateway you were not touching then answers 401 to everything, and its
    /// browser drops to the login screen the next time anything asks — which
    /// reads as a session bug in whatever you were actually testing. Testing rxa
    /// session takeover needs two gateways, so this is not a rare corner.
    ///
    /// `<label>.localhost` because every label under `.localhost` resolves to
    /// loopback without DNS (RFC 6761) and is a *distinct* cookie origin, so two
    /// gateways become two independent logins in one browser.
    ///
    /// Never reachable in a deployment: [`AppConfig::dev_hostname`] redirects only
    /// a request whose own `Host` is a loopback name, so a gateway behind a real
    /// hostname or address ignores this however it is set.
    pub dev_subdomain: Option<String>,
}

/// The default display name when `[server].branding` is unset.
pub const DEFAULT_BRANDING: &str = "remotex";

/// The optional `[rxa]` block: this gateway's identity on the `rxa` protocol.
///
/// One identity for the whole server, not one per target — the
/// `[Interface]`/`[Peer]` split WireGuard makes. Every Mac agent is configured
/// with the same gateway public key, and each target names the agent's.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RxaSection {
    /// This gateway's private key (`rxgs…`), generated with `remotex gen-key`.
    /// Its public half — printed by `remotex rxa-pubkey` — is what goes into
    /// each Mac agent's `authorized_gateways` file.
    ///
    /// Required as soon as any target is protocol `rxa`. Kept when there are
    /// none: this is the machine's identity rather than a per-target
    /// credential, and dropping the last `rxa` target should not mean minting a
    /// new one to add the next.
    pub private_key: String,
}

/// The parsed TOML file, before a target is selected.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub rxa: RxaSection,
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
    /// `<label>.localhost` to send a loopback browser to, from
    /// `[server].dev_subdomain`. `None` disables the redirect entirely.
    ///
    /// Stored as the whole hostname rather than the label so the one place that
    /// validated it is the only place that builds it — a redirect target
    /// assembled at the point of use is one that can be assembled wrongly.
    pub dev_hostname: Option<String>,
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
        // Keys are validated here rather than at connect time: a mistyped one
        // should fail on startup with the CRC's "transcription typo" hint (or,
        // for the wrong kind of key, a message naming what was pasted), not as
        // a handshake rejection the first time someone picks the target.
        let rxa_private_key = config.rxa.private_key.trim();
        if config.targets.iter().any(|t| t.protocol == Protocol::Rxa) {
            anyhow::ensure!(
                !rxa_private_key.is_empty(),
                "a target is protocol \"rxa\" but [rxa].private_key is unset — \
                 generate one with `remotex gen-key`"
            );
            rxa_proto::key::parse_private(rxa_proto::key::Role::Gateway, rxa_private_key)
                .map_err(|e| anyhow::anyhow!("[rxa].private_key is invalid: {e}"))?;
        }
        for target in &config.targets {
            if target.protocol == Protocol::Rxa {
                let agent_public_key = target.agent_public_key.trim();
                anyhow::ensure!(
                    !agent_public_key.is_empty(),
                    "target {:?} is protocol \"rxa\" but has no agent_public_key — \
                     read it off that Mac with `remotex-agent --public-key`, or from \
                     the agent's Settings",
                    target.name
                );
                rxa_proto::key::parse_public(rxa_proto::key::Role::Agent, agent_public_key)
                    .map_err(|e| {
                        anyhow::anyhow!("target {:?} has an invalid agent_public_key: {e}", target.name)
                    })?;
                // RXA resize capability also depends on agent-owned display state,
                // which cannot be validated from gateway configuration.
            } else {
                anyhow::ensure!(
                    target.agent_public_key.is_empty(),
                    "target {:?} is protocol {:?} but sets agent_public_key, which only \
                     \"rxa\" targets use",
                    target.name,
                    target.protocol.name()
                );
            }
            // Audio is the mirror image of `resize`: refused outright rather than
            // accepted and left inert, because there is nothing on the other side
            // of the protocol to be uncertain about. RFB has no audio channel and
            // the Mac agent captures no sound, so this key could never do anything
            // for them however they were configured.
            anyhow::ensure!(
                !target.audio || target.protocol == Protocol::Rdp,
                "target {:?} is protocol {:?} but sets audio, which only \"rdp\" targets \
                 support — MS-RDPEA is the one audio channel the gateway speaks",
                target.name,
                target.protocol.name()
            );
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
                    // Still rejected, and no longer for the reason `rxa` used to
                    // reject it — `rxa` accepts it now. The difference is what is
                    // behind the protocol: an agent can make a display for a
                    // client to resize, while a Mac reached over Screen Sharing
                    // has only its own screens, whose resolution is set on the
                    // Mac. Apple's server accepts the resize negotiation and then
                    // ignores every request, so the key would promise a control
                    // that does nothing at all.
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
                // The protocols without subtypes are named rather than left to a
                // catch-all, so that a second VNC subtype cannot land here and
                // be told it belongs to another protocol. Adding one stops the
                // build until this match says what it means.
                (protocol @ (Protocol::Rdp | Protocol::Rxa), Some(subtype)) => anyhow::bail!(
                    "target {:?} is protocol {:?} and sets subtype {:?}, which only \"vnc\" \
                     targets have",
                    target.name,
                    protocol.name(),
                    subtype.name()
                ),
                (Protocol::Rdp | Protocol::Rxa, None) => {}
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
    pub fn resolve(mut self) -> anyhow::Result<AppConfig> {
        // One server identity, handed to every target that speaks the protocol
        // it belongs to — see `TargetConfig::gateway_private_key` for why it
        // rides along on the target rather than beside it.
        let private_key = self.rxa.private_key.trim().to_owned();
        for target in &mut self.targets {
            if target.protocol == Protocol::Rxa {
                target.gateway_private_key = private_key.clone();
            }
        }
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
            dev_hostname: self
                .server
                .dev_subdomain
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(dev_hostname)
                .transpose()
                .context("invalid [server].dev_subdomain")?,
        })
    }
}

/// `<label>.localhost`, refusing anything that is not a single DNS label.
///
/// The check is what makes the redirect target unforgeable: a `Location` built
/// from an unvalidated string could name any host at all, and this one is
/// assembled from a label that has been proved to contain no dot, no slash, no
/// colon and no credentials. So the target is always some name under
/// `.localhost`, which by RFC 6761 can only be loopback.
///
/// Length is bounded at 63, the DNS label limit, for the same reason the shape is
/// checked rather than trusted: a name nothing can resolve is a redirect loop
/// waiting to happen, and a config file is where it should be caught.
fn dev_hostname(label: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        label.len() <= 63,
        "{label:?} is longer than a DNS label may be (63 characters)"
    );
    anyhow::ensure!(
        label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "{label:?} must be one DNS label — ASCII letters, digits and hyphens only, \
         and no dots (it is used as <label>.localhost)"
    );
    anyhow::ensure!(
        !label.starts_with('-') && !label.ends_with('-'),
        "{label:?} may not start or end with a hyphen"
    );
    Ok(format!("{label}.localhost"))
}

/// Load the config file: the explicit `--config` path, or the global
/// `<prefix>/etc/remotex.toml` of the installed layout. Returns the parsed file
/// and the path it came from.
pub fn load(explicit: Option<&Path>) -> anyhow::Result<(ConfigFile, PathBuf)> {
    let path = config_path(explicit)?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config =
        ConfigFile::parse(&text).with_context(|| format!("in config file {}", path.display()))?;
    Ok((config, path))
}

/// Read `[rxa].private_key` out of a config file, ignoring everything else in
/// it.
///
/// Deliberately *not* [`load`]: `remotex rxa-pubkey` has to work before the
/// config is wholly valid, because pairing is a cycle otherwise. A target's
/// `agent_public_key` is read off a Mac that has not been paired yet, and that
/// Mac is paired with the value this prints — so demanding every target already
/// carry a valid key would mean neither end could ever be configured first.
pub fn load_rxa_private_key(explicit: Option<&Path>) -> anyhow::Result<(String, PathBuf)> {
    /// Just the one section. No `deny_unknown_fields`: the rest of the file is
    /// none of this function's business, including whatever is wrong with it.
    #[derive(Deserialize)]
    struct RxaOnly {
        #[serde(default)]
        rxa: RxaSection,
    }

    let path = config_path(explicit)?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let file: RxaOnly = toml::from_str(&text)
        .with_context(|| format!("invalid TOML in config file {}", path.display()))?;
    Ok((file.rxa.private_key.trim().to_owned(), path))
}

/// Which config file to read: the one named, or the installed one.
fn config_path(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    match explicit {
        Some(path) => Ok(path.to_path_buf()),
        None => installed_config_path().context(
            "no --config given and not running from an installed prefix \
             (<prefix>/versions/<version>/bin/remotex) — pass --config <path>",
        ),
    }
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
        assert!(!t.audio, "remote audio is opt-in");
    }

    /// A config with one `[server]` line under test.
    fn with_server(line: &str) -> String {
        format!(
            r#"
            [server]
            {line}
            {}

            [[targets]]
            name = "one"
            protocol = "rdp"
            host = "192.0.2.10"
            "#,
            site_passwd_line()
        )
    }

    fn resolved(line: &str) -> AppConfig {
        ConfigFile::parse(&with_server(line))
            .unwrap()
            .resolve()
            .unwrap()
    }

    // The dev-only hostname. Its validation is the reason the redirect target is
    // unforgeable: a `Location` is built from this and nothing else, so a value
    // carrying a dot, a slash, a colon or credentials would point somewhere that is
    // not loopback at all.
    #[test]
    fn a_dev_subdomain_becomes_one_label_under_localhost() {
        assert_eq!(
            resolved(r#"dev_subdomain = "a""#).dev_hostname.as_deref(),
            Some("a.localhost")
        );
        // Unset, and whitespace-only, both disable it — as `branding` does.
        assert_eq!(resolved("").dev_hostname, None);
        assert_eq!(resolved(r#"dev_subdomain = "  ""#).dev_hostname, None);
        // Trimmed, so a stray space cannot become part of a hostname.
        assert_eq!(
            resolved(r#"dev_subdomain = "  b  ""#).dev_hostname.as_deref(),
            Some("b.localhost")
        );
        // Digits and inner hyphens are legal in a DNS label.
        assert_eq!(
            resolved(r#"dev_subdomain = "gw-2""#).dev_hostname.as_deref(),
            Some("gw-2.localhost")
        );
    }

    #[test]
    fn a_dev_subdomain_that_is_not_one_label_is_refused() {
        for bad in [
            // A dot would move the name out from under `.localhost` entirely,
            // which is the whole of what keeps the target on loopback.
            "a.b",
            "evil.example.com",
            "a/b",
            "a:8080",
            "user@host",
            "-a",
            "a-",
            "a b",
            "aä",
            // 63 is the DNS label ceiling; longer is a name nothing resolves,
            // which would be a redirect loop rather than a working gateway.
            &"a".repeat(64),
        ] {
            let err = ConfigFile::parse(&with_server(&format!("dev_subdomain = {bad:?}")))
                .and_then(ConfigFile::resolve)
                .expect_err("should be refused: {bad:?}");
            assert!(
                format!("{err:#}").contains("dev_subdomain"),
                "{bad:?} was refused without naming the key: {err:#}"
            );
        }
        assert_eq!(
            resolved(&format!("dev_subdomain = {:?}", "a".repeat(63)))
                .dev_hostname
                .as_deref(),
            Some(&*format!("{}.localhost", "a".repeat(63)))
        );
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

    use rxa_proto::key::{self, Role};

    /// A fresh gateway identity, as `remotex gen-key` prints it.
    fn gateway_private_key() -> String {
        key::generate_private(Role::Gateway)
    }

    /// A fresh Mac's public key, as its agent reports it.
    fn agent_public_key() -> String {
        key::public_text_of(Role::Agent, &key::generate_private(Role::Agent)).unwrap()
    }

    /// An `rxa` config with both halves of a valid pairing: a `[rxa]` block and
    /// one target. `extra` adds further target keys.
    fn rxa_toml(extra: &str) -> String {
        rxa_toml_keyed(&gateway_private_key(), &agent_public_key(), extra)
    }

    /// [`rxa_toml`] with the two key lines spelled out, for the tests that are
    /// about what happens when one of them is wrong.
    fn rxa_toml_keyed(private_key: &str, agent_public_key: &str, extra: &str) -> String {
        format!(
            r#"
            [server]
            {}

            [rxa]
            private_key = "{private_key}"

            [[targets]]
            name = "mac"
            protocol = "rxa"
            host = "mac.local"
            agent_public_key = "{agent_public_key}"
            {extra}
            "#,
            site_passwd_line()
        )
    }

    #[test]
    fn rxa_target_parses_with_its_own_default_port() {
        let (private_key, public_key) = (gateway_private_key(), agent_public_key());
        let config = ConfigFile::parse(&rxa_toml_keyed(&private_key, &public_key, ""))
            .unwrap()
            .resolve()
            .unwrap();
        let target = &config.targets[0];
        assert_eq!(target.protocol, Protocol::Rxa);
        assert_eq!(target.protocol.name(), "rxa");
        // Adjacent to the web server's 52380, and not 3389 or 5900.
        assert_eq!(target.port, 52381);
        assert_eq!(target.agent_public_key, public_key);
        // `resolve` fans the one server identity out to the targets that speak
        // the protocol, which is how the engine gets it (see the field's docs).
        assert_eq!(target.gateway_private_key, private_key);
        assert!(!target.resize, "resize is opt-in for rxa too");

        // An explicit port still wins.
        let config = ConfigFile::parse(&rxa_toml("port = 52999"))
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(config.targets[0].port, 52999);
    }

    // A non-rxa target gets no identity to carry: it would be an unused copy of
    // a private key on a struct that is cloned per session.
    #[test]
    fn resolve_leaves_the_gateway_key_off_targets_that_cannot_use_it() {
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [rxa]
            private_key = "{}"

            [[targets]]
            name = "pc"
            protocol = "rdp"
            host = "10.0.0.5"
            username = "u"
            password = "p"
            "#,
            site_passwd_line(),
            gateway_private_key()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].gateway_private_key.is_empty());
    }

    // A mistyped key must fail on startup with the CRC's hint, not as an opaque
    // handshake rejection the first time someone picks the target.
    #[test]
    fn rxa_keys_are_validated_at_parse_time() {
        let missing = rxa_toml_keyed(&gateway_private_key(), "", "");
        let err = ConfigFile::parse(&missing).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no agent_public_key") && msg.contains("--public-key"),
            "{msg}"
        );

        let err = ConfigFile::parse(&rxa_toml_keyed(&gateway_private_key(), "rxapnope", ""))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid agent_public_key"),
            "{err:#}"
        );

        // A single-character typo in an otherwise well-formed key.
        let mut chars: Vec<char> = agent_public_key().chars().collect();
        chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        let err = ConfigFile::parse(&rxa_toml_keyed(&gateway_private_key(), &typo, "")).unwrap_err();
        assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    }

    #[test]
    fn an_rxa_target_without_a_gateway_identity_is_rejected() {
        let err = ConfigFile::parse(&rxa_toml_keyed("", &agent_public_key(), "")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("[rxa].private_key") && msg.contains("gen-key"),
            "{msg}"
        );

        let err = ConfigFile::parse(&rxa_toml_keyed("rxgsnope", &agent_public_key(), "")).unwrap_err();
        assert!(
            format!("{err:#}").contains("[rxa].private_key is invalid"),
            "{err:#}"
        );
    }

    // The whole reason the role is in the prefix. Both of these are well-formed
    // keys that would otherwise fail as a handshake rejection with nothing to
    // say which end was misconfigured.
    #[test]
    fn a_key_of_the_wrong_kind_is_named_rather_than_failing_at_the_handshake() {
        let private_key = gateway_private_key();
        let gateway_public = key::public_text_of(Role::Gateway, &private_key).unwrap();

        // The two public keys are on screen together while pairing, so this is
        // the swap most easily made.
        let err = ConfigFile::parse(&rxa_toml_keyed(&private_key, &gateway_public, "")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("this is a gateway public key"), "{msg}");

        // And the other direction: a Mac's own private key where the gateway's
        // belongs.
        let agent_private = key::generate_private(Role::Agent);
        let err = ConfigFile::parse(&rxa_toml_keyed(&agent_private, &agent_public_key(), ""))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("this is an agent private key"), "{msg}");
    }

    // A server keeps its identity across a config that has no rxa targets in it
    // today: it is the machine's, not the target's, and re-minting it to add the
    // next Mac would mean re-pairing every other one.
    #[test]
    fn a_gateway_identity_with_no_rxa_targets_is_kept() {
        ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [rxa]
            private_key = "{}"

            [[targets]]
            name = "box"
            protocol = "vnc"
            host = "10.0.0.4"
            vnc_password = "hunter2"
            "#,
            site_passwd_line(),
            gateway_private_key()
        ))
        .unwrap();
    }

    #[test]
    fn an_agent_public_key_on_a_non_rxa_target_is_rejected() {
        // Silently ignoring it would leave someone believing a VNC target was
        // authenticated by a key it never uses.
        let err = ConfigFile::parse(&format!(
            r#"
            [[targets]]
            name = "one"
            protocol = "vnc"
            host = "h"
            agent_public_key = "{}"
            "#,
            agent_public_key()
        ))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("agent_public_key") && msg.contains("rxa"), "{msg}");
    }

    // Pairing is a cycle if this does not hold: the value `remotex rxa-pubkey`
    // prints is what a Mac needs before it can report the `agent_public_key`
    // this config is waiting for, so reading the gateway's own key must not
    // require a config that is already wholly valid.
    #[test]
    fn the_gateway_private_key_is_readable_from_a_config_that_does_not_parse() {
        let dir = std::env::temp_dir().join(format!("remotex-rxa-pubkey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remotex.toml");
        let private_key = gateway_private_key();
        // An rxa target with no agent key yet — exactly the state a half-set-up
        // config is in, and one `ConfigFile::parse` rightly refuses.
        std::fs::write(
            &path,
            rxa_toml_keyed(&private_key, "", ""),
        )
        .unwrap();

        assert!(ConfigFile::parse(&std::fs::read_to_string(&path).unwrap()).is_err());
        let (read, from) = load_rxa_private_key(Some(&path)).unwrap();
        assert_eq!(read, private_key);
        assert_eq!(from, path);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // `gateway_private_key` is populated by `resolve`, never read from the file:
    // a hand-set one would be a second, silently-ignored opinion about the
    // server's identity.
    #[test]
    fn a_target_cannot_set_the_gateway_private_key_itself() {
        let err = ConfigFile::parse(&rxa_toml(&format!(
            "gateway_private_key = \"{}\"",
            gateway_private_key()
        )))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("gateway_private_key"),
            "{err:#}"
        );
    }

    // Accepted, and only half of what enables the control: the other half is that
    // the display being shared is one the agent made, which is a per-session fact
    // this file cannot see. A Mac whose agent has no such display gets an inert
    // flag rather than an error — refusing here would reject a config that is
    // correct for every Mac but that one.
    #[test]
    fn resize_is_accepted_on_an_rxa_target() {
        let config = ConfigFile::parse(&rxa_toml("resize = true"))
            .unwrap()
            .resolve()
            .unwrap();
        assert!(config.targets[0].resize);
    }

    #[test]
    fn clipboard_is_accepted_for_every_protocol() {
        let config = ConfigFile::parse(&rxa_toml("clipboard = true"))
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

    /// Unlike `clipboard`, which every engine supports, and unlike `resize` on
    /// `rxa`, which is accepted because this file cannot see the other half of
    /// the answer: there is no audio channel behind RFB or the Mac agent at all,
    /// so the key is refused where it could never do anything.
    #[test]
    fn audio_is_accepted_for_rdp_and_refused_for_the_other_protocols() {
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "10.0.0.5"
            audio = true
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].audio);

        for (protocol, extra) in [
            ("vnc", String::new()),
            ("rxa", format!("agent_public_key = \"{}\"", agent_public_key())),
        ] {
            let err = ConfigFile::parse(&format!(
                r#"
                [server]
                {}

                [rxa]
                private_key = "{}"

                [[targets]]
                name = "nope"
                protocol = "{protocol}"
                host = "10.0.0.6"
                audio = true
                {extra}
                "#,
                site_passwd_line(),
                gateway_private_key()
            ))
            .unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains("audio"), "{rendered}");
            assert!(rendered.contains(protocol), "{rendered}");
        }
    }
}
