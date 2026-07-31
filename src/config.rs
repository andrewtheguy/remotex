//! Global TOML configuration: one `[server]` block and `[[targets]]` profiles.
//! Only the selected config file is read; target credentials remain server-side.
//!
//! One schema, read by two kinds of gateway — see [`Audience`]. The `[[targets]]`
//! half is identical for both, because a target is a target; `[server]` belongs to
//! the one a browser reaches.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

use crate::auth::{EmbeddedToken, GatewayAuth, SitePasswd};

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
/// (src/rdp.rs), `vnc` via the built-in RFB client (src/vnc.rs). A Mac is
/// reached as a `vnc` target with `subtype = "ard"` — macOS Screen Sharing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Rdp,
    Vnc,
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

/// How a target's tiles are encoded — the quality *strategy*, the first of the
/// two render axes (the second is [`RenderSubtype`], the codec, and a lossy
/// strategy also reads [`TargetConfig::render_quality`]). Two flat sibling keys
/// rather than a nested table, matching the rest of the target schema.
///
/// Only the two implemented strategies are variants; a config naming a planned
/// one (`adaptive`, quality that follows motion or link speed) is refused by
/// serde with the list of what is accepted. See docs/proposals/quality-dial.md.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderType {
    /// Lossless: every tile is PNG, byte-for-byte what the gateway has always
    /// sent. The default, so an existing config behaves exactly as before. Pairs
    /// only with [`RenderSubtype::Png`].
    #[default]
    Full,
    /// One quality for the whole session, set by [`TargetConfig::render_quality`]
    /// and never varied. Pairs with a lossy codec ([`RenderSubtype::Jpeg`] or
    /// [`RenderSubtype::Webp`]).
    FixedQuality,
}

/// The codec a target's tiles are encoded with — the second render axis, paired
/// with [`RenderType`]. Only implemented codecs are variants; a planned one
/// (`adaptive-jpeg`, a per-tile PNG/JPEG classifier; `video`) is refused by serde.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderSubtype {
    /// Lossless PNG. The default, and the only codec [`RenderType::Full`] takes.
    #[default]
    Png,
    /// Baseline JPEG at [`TargetConfig::render_quality`]. Every tile goes to JPEG
    /// — there is no content classifier — so flat UI and text soften along with
    /// photographic content. That is the trade the fixed dial makes.
    Jpeg,
    /// WebP at [`TargetConfig::render_quality`] — the same fixed-quality, no-
    /// classifier trade as [`Self::Jpeg`], but typically ~30% fewer bytes at a
    /// matched quality. Both clients decode it natively.
    Webp,
}

/// The tile encoder an engine uses, resolved from a target's render dial by
/// [`TargetConfig::tile_codec`]. The two axes and the quality collapse to this,
/// so `rdp::run` / `vnc::run` and [`crate::encode::TileSink`] match on one value
/// and never touch the config enums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileCodec {
    /// Lossless PNG — the default path.
    Png,
    /// JPEG at the given quality (1–100).
    Jpeg(u8),
    /// WebP at the given quality (1–100).
    Webp(u8),
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
    /// Remote-desktop protocol: `"rdp"` or `"vnc"`. Required — each target must
    /// say what it speaks.
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
    /// (3389 for RDP, 5900 for VNC) — normalized in [`ConfigFile::parse`].
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
    /// should get.
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
    /// continuously or resizes when asked is that client's own choice.
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
    /// Supported by both engines, though what reaches the far side differs:
    /// `vnc` uses RFB `ServerCutText`/`ClientCutText` and is latin-1, so
    /// anything outside it becomes `?`; `rdp` uses the MS-RDPECLIP virtual
    /// channel with `CF_UNICODETEXT`, UTF-8 end to end.
    #[serde(default)]
    pub clipboard: bool,
    /// Negotiate RDP audio at connect. Packets are sent only while the attached
    /// client subscribes. Rejected for VNC.
    #[serde(default)]
    pub audio: bool,
    /// Quality *strategy* for this target's tiles. Defaults to [`RenderType::Full`]
    /// (lossless PNG), so an unset target is byte-identical to before the dial
    /// existed. Validated against [`Self::render_subtype`] and [`Self::render_quality`]
    /// in [`ConfigFile::parse_with`]. Works for both RDP and VNC.
    #[serde(default)]
    pub render_type: RenderType,
    /// Codec for this target's tiles. Defaults to [`RenderSubtype::Png`]. The
    /// legal pairing with [`Self::render_type`] is enforced at parse time.
    #[serde(default)]
    pub render_subtype: RenderSubtype,
    /// Fixed quality (1–100) for [`RenderType::FixedQuality`], applied by whichever
    /// lossy codec [`Self::render_subtype`] selects ([`RenderSubtype::Jpeg`] or
    /// [`RenderSubtype::Webp`]). Required for that strategy and refused for
    /// [`RenderType::Full`], which is lossless and has no dial. `None` (unset) is
    /// the default.
    #[serde(default)]
    pub render_quality: Option<u8>,
}

impl TargetConfig {
    /// The tile encoder to use for this target. This is the whole of the render
    /// dial as the engines see it: the two axes and the quality collapse to one
    /// [`TileCodec`], so `rdp::run` / `vnc::run` need not know the config enums.
    ///
    /// A lossy codec carries its quality, which [`ConfigFile::parse_with`] has
    /// already guaranteed is present and in range for a `fixed-quality` target;
    /// the `None` arm falls back to lossless PNG rather than trusting that here.
    pub fn tile_codec(&self) -> TileCodec {
        match (self.render_type, self.render_subtype, self.render_quality) {
            (RenderType::FixedQuality, RenderSubtype::Jpeg, Some(q)) => TileCodec::Jpeg(q),
            (RenderType::FixedQuality, RenderSubtype::Webp, Some(q)) => TileCodec::Webp(q),
            _ => TileCodec::Png,
        }
    }
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
    // No `branding` here: it is a top-level key now (see `ConfigFile::branding`),
    // because `remotex.app`'s config has no `[server]` block to hold it and one
    // value with two spellings is one of them going stale. `deny_unknown_fields`
    // refuses a file that still has it here.
    /// **Development only.** A label to give this gateway its own hostname on
    /// loopback: a browser arriving at `127.0.0.1`, `::1` or `localhost` is
    /// redirected to `<label>.localhost`, keeping the port and path.
    ///
    /// It exists for one problem, which has no other clean answer: a cookie is
    /// scoped by *host* and ignores the port, so two gateways on one machine
    /// share `remotex_session` and each login silently evicts the other. The
    /// gateway you were not touching then answers 401 to everything, and its
    /// browser drops to the login screen the next time anything asks — which
    /// reads as a session bug in whatever you were actually testing. Testing
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

/// Who a config file is for, and therefore which rules it is held to.
///
/// The difference is not cosmetic — each audience makes a demand the other one
/// cannot meet — which is why this is a parameter of parsing rather than something
/// checked later by whoever happens to remember to:
///
/// - a [`Self::Served`] gateway is useless without a target to offer and a
///   credential to guard it, and it is told where to listen;
/// - an [`Self::Embedded`] one is started by `remotex.app` with the port, the
///   secret and the (absent) web root decided by the app, so a `[server]` block
///   could only contradict it — and it must come up with **no targets at all**,
///   because that is what a first launch has and the picker's job is to say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// `remotex serve`: a browser's gateway, and the macOS agent's peer.
    Served,
    /// `remotex serve-embedded`: the gateway inside `remotex.app`.
    Embedded,
}

/// The parsed TOML file, before a target is selected.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// `None` when the file has no `[server]` block at all, which is what an
    /// embedded gateway's config must look like — distinguishing "absent" from
    /// "present and empty" is the whole reason this is an `Option`.
    #[serde(default)]
    pub server: Option<ServerSection>,
    /// Display name of this gateway: the browser's login screen, interstitials and
    /// tab title, and in `remotex.app` the heading above its target list, its window
    /// title and its launch screen.
    ///
    /// Top-level rather than in `[server]`, and it is the **only** place to set it.
    /// `remotex.app`'s config has no `[server]` block at all
    /// ([`Audience::Embedded`]), so a key that lived there could not name the app —
    /// and accepting both spellings would be two places to write one value, with the
    /// loser losing silently. `deny_unknown_fields` refuses a file that still has it
    /// under `[server]`, which is the whole of the migration.
    ///
    /// Defaults to [`DEFAULT_BRANDING`]; whitespace-only is treated as absent.
    #[serde(default)]
    pub branding: Option<String>,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

/// Resolved runtime configuration: the web server plus every target profile it
/// serves (the browser picks one after login).
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Host/interface the web server binds to.
    pub host: String,
    /// Port the web server binds to. `0` asks the kernel for an ephemeral one,
    /// which is what an embedded gateway does — the port it got is then read off
    /// the listener and told to its client, never guessed.
    pub port: u16,
    /// Directory holding the built frontend (index.html + assets), served from
    /// disk. Defaults to [`default_static_dir`].
    ///
    /// `None` means **there is no web UI**, and it is not a fallback for a
    /// directory that turned out to be missing: an embedded gateway ships no SPA
    /// on purpose, so it serves none rather than 404ing its way through one. Every
    /// path outside `/api` and `/ws` is a 404 (see [`crate::server::router`]).
    pub static_dir: Option<PathBuf>,
    /// Every target profile this process serves; the post-login picker selects
    /// one. Non-empty for [`Audience::Served`]; possibly empty for an embedded
    /// gateway, whose client shows "no targets are configured" instead.
    pub targets: Vec<TargetConfig>,
    /// What gets a request past the door: a login, or the embedded client's token.
    pub auth: GatewayAuth,
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
    /// Parse a browser gateway's config. See [`Self::parse_with`] for the other
    /// audience.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Self::parse_with(text, Audience::Served)
    }

    /// Parse a config file for `audience`.
    ///
    /// Everything about the targets is checked identically for both — the two
    /// audiences differ only in what they may say about the *server*, and in
    /// whether having nothing to offer yet is an error or a first launch.
    pub fn parse_with(text: &str, audience: Audience) -> anyhow::Result<Self> {
        let mut config: ConfigFile = toml::from_str(text).context("invalid TOML config")?;
        // An omitted port deserializes as 0 (never a valid target port), which
        // resolves here to the protocol's standard port.
        for target in &mut config.targets {
            if target.port == 0 {
                target.port = target.protocol.default_port();
            }
        }
        if audience == Audience::Embedded {
            // Refused rather than ignored, and named as a whole block rather than
            // key by key: every one of them is a decision the app has already made
            // for this gateway — an ephemeral loopback port it reads back off the
            // socket, no web root because no SPA ships in the bundle, and a token
            // instead of a login. A key that is quietly overridden is worse than
            // one that is refused: it reads as configuration and behaves as
            // decoration.
            anyhow::ensure!(
                config.server.is_none(),
                "this config is remotex.app's own and may not have a [server] block: \
                 the app decides where its gateway listens, serves no web UI, and \
                 authenticates itself. Only branding and [[targets]] belong here"
            );
        } else {
            anyhow::ensure!(
                !config.targets.is_empty(),
                "config has no [[targets]] — at least one target profile is required"
            );
        }
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
        for target in &config.targets {
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
                    // Rejected because only *standard* macOS Screen Sharing is
                    // supported today: it shares the Mac's real screens, whose
                    // resolution is set on the Mac. Dynamic resize over ARD is a
                    // high-performance-mode feature — Screen Sharing can spin up a
                    // resizable virtual display, like RDP — and is not implemented
                    // yet (see docs/roadmap.md).
                    anyhow::ensure!(
                        !target.resize,
                        "target {:?} is subtype \"ard\" and sets resize, which this gateway \
                         does not support yet: standard macOS Screen Sharing shares the Mac's \
                         real screens at the size set on the Mac, and dynamic resize is \
                         high-performance ARD, which is not implemented yet",
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
                (Protocol::Rdp, Some(subtype)) => anyhow::bail!(
                    "target {:?} is protocol \"rdp\" and sets subtype {:?}, which only \"vnc\" \
                     targets have",
                    target.name,
                    subtype.name()
                ),
                (Protocol::Rdp, None) => {}
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
            // The render dial has two axes and they are validated together,
            // because only some pairings mean anything and `render_quality`
            // belongs to exactly one of them. The match is exhaustive so a future
            // variant cannot be added without deciding what it pairs with here.
            match (target.render_type, target.render_subtype) {
                (RenderType::Full, RenderSubtype::Png) => {
                    anyhow::ensure!(
                        target.render_quality.is_none(),
                        "target {:?} sets render_quality, which render_type \"full\" has no \
                         use for — it is lossless PNG. Set render_type = \"fixed-quality\" \
                         with a lossy render_subtype (\"jpeg\" or \"webp\") to choose a quality",
                        target.name
                    );
                }
                (RenderType::FixedQuality, RenderSubtype::Jpeg | RenderSubtype::Webp) => {
                    let q = target.render_quality.with_context(|| format!(
                        "target {:?} is render_type \"fixed-quality\" but sets no \
                         render_quality — it needs one, an integer 1–100",
                        target.name
                    ))?;
                    anyhow::ensure!(
                        (1..=100).contains(&q),
                        "target {:?} sets render_quality = {q}, which is out of range — it \
                         must be 1–100",
                        target.name
                    );
                }
                (RenderType::Full, RenderSubtype::Jpeg | RenderSubtype::Webp) => anyhow::bail!(
                    "target {:?} sets render_type \"full\" with a lossy render_subtype: \
                     \"full\" is lossless and pairs only with render_subtype \"png\". Use \
                     render_type = \"fixed-quality\" for JPEG or WebP",
                    target.name
                ),
                (RenderType::FixedQuality, RenderSubtype::Png) => anyhow::bail!(
                    "target {:?} sets render_type \"fixed-quality\" with render_subtype \
                     \"png\": PNG is lossless and has no quality dial. Use render_subtype = \
                     \"jpeg\" or \"webp\", or render_type = \"full\" to stay lossless",
                    target.name
                ),
            }
        }
        Ok(config)
    }

    /// Resolve the runtime configuration of the gateway inside `remotex.app`:
    /// loopback, an ephemeral port, no web UI, and a freshly minted token.
    ///
    /// Every one of those is a constant here rather than a default that
    /// `[server]` could override, which is what [`Audience::Embedded`] enforces on
    /// the way in. `branding` is the one thing such a config *may* say about the
    /// gateway itself, because it is about the app rather than about the server: it
    /// names a window, not a deployment, and two instances on one Mac are easier to
    /// tell apart if they can be called different things.
    pub fn resolve_embedded(self, token: EmbeddedToken) -> anyhow::Result<AppConfig> {
        Ok(AppConfig {
            // Not `localhost`: that name resolves to both loopbacks and the client
            // is told one port on one address. The app connects to 127.0.0.1.
            host: "127.0.0.1".to_owned(),
            port: 0,
            static_dir: None,
            targets: self.targets,
            auth: GatewayAuth::Token(token),
            branding: Self::resolve_branding(self.branding.as_deref()),
            dev_hostname: None,
        })
    }

    /// The display name, or [`DEFAULT_BRANDING`].
    ///
    /// Whitespace-only counts as absent: a heading of one space is not a name
    /// somebody meant to give. Shared by both audiences because it is one key now,
    /// and a second copy of this three-line rule is how the two would come to differ.
    fn resolve_branding(configured: Option<&str>) -> String {
        configured
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_BRANDING)
            .to_owned()
    }

    /// Resolve the runtime configuration: validate the web-login credential and
    /// carry over every target profile (the browser picks one after login).
    pub fn resolve(self) -> anyhow::Result<AppConfig> {
        let server = self.server.unwrap_or_default();
        let site_passwd = server
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
            host: server.host.unwrap_or_else(|| "127.0.0.1".to_owned()),
            port: server.port.unwrap_or(52380),
            static_dir: Some(server.static_dir.unwrap_or_else(default_static_dir)),
            // Non-empty is guaranteed by `parse`.
            targets: self.targets,
            auth: GatewayAuth::Login(site_passwd),
            branding: Self::resolve_branding(self.branding.as_deref()),
            dev_hostname: server
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
        let GatewayAuth::Login(site_passwd) = &config.auth else {
            panic!("a served gateway logs in");
        };
        assert_eq!(site_passwd.username(), "admin");
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

        // Set → carried through, trimmed. Top-level, which is the only place it
        // lives: an app instance's config has no [server] block to hold it.
        let toml = format!(
            r#"
            branding = "  Acme Remote  "

            [server]
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
        assert_eq!(config.static_dir, Some(PathBuf::from("/srv/web")));
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

    #[test]
    fn render_defaults_to_lossless_png() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_type, RenderType::Full);
        assert_eq!(t.render_subtype, RenderSubtype::Png);
        assert_eq!(t.render_quality, None);
    }

    #[test]
    fn fixed_quality_jpeg_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "fixed-quality"
            render_subtype = "jpeg"
            render_quality = 60
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_type, RenderType::FixedQuality);
        assert_eq!(t.render_subtype, RenderSubtype::Jpeg);
        assert_eq!(t.render_quality, Some(60));
        assert_eq!(t.tile_codec(), TileCodec::Jpeg(60));
    }

    #[test]
    fn fixed_quality_webp_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "fixed-quality"
            render_subtype = "webp"
            render_quality = 50
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_subtype, RenderSubtype::Webp);
        assert_eq!(t.tile_codec(), TileCodec::Webp(50));
    }

    #[test]
    fn full_with_webp_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_subtype = "webp"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("lossless"), "{err:#}");
    }

    #[test]
    fn fixed_quality_without_a_quality_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "fixed-quality"
            render_subtype = "jpeg"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_quality"), "{err:#}");
    }

    #[test]
    fn a_render_quality_out_of_range_is_rejected() {
        for q in ["0", "101"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                render_type = "fixed-quality"
                render_subtype = "jpeg"
                render_quality = {q}
                "#
            );
            let err = ConfigFile::parse(&toml).unwrap_err();
            assert!(format!("{err:#}").contains("1–100"), "q={q}: {err:#}");
        }
    }

    #[test]
    fn render_quality_on_full_is_rejected() {
        // render_type/subtype default to full/png, so a stray quality has nothing
        // to apply to.
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_quality = 50
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("full"), "{err:#}");
    }

    #[test]
    fn mismatched_render_axes_are_rejected() {
        // full + jpeg: full is lossless.
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "full"
            render_subtype = "jpeg"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("lossless"), "{err:#}");

        // fixed-quality + png: PNG has no dial.
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "fixed-quality"
            render_subtype = "png"
            render_quality = 50
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("quality dial"), "{err:#}");
    }

    #[test]
    fn an_unknown_render_type_names_the_supported_ones() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "adaptive"
            "#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("full") && msg.contains("fixed-quality"), "{msg}");
    }

    // Nothing in a target says what the remote runs. The engines discover it
    // (src/vnc.rs asks the RFB greeting), so a config that tried to declare it
    // is a typo, not a supported knob.
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

        // Resize is rejected: only standard screen sharing is supported today, and
        // dynamic resize over ARD is a high-performance feature, not implemented yet.
        let err =
            ard("username = \"andrew\"\npassword = \"h\"\nresize = true").unwrap_err();
        assert!(format!("{err:#}").contains("does not support yet"), "{err:#}");

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

    #[test]
    fn clipboard_is_accepted_for_both_protocols() {
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
        // be refused here; the flag is now accepted for both engines.
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

    /// Unlike `clipboard`, which both engines support: there is no audio channel
    /// behind RFB at all, so the key is refused where it could never do anything.
    #[test]
    fn audio_is_accepted_for_rdp_and_refused_for_vnc() {
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

        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "nope"
            protocol = "vnc"
            host = "10.0.0.6"
            audio = true
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("audio"), "{rendered}");
        assert!(rendered.contains("vnc"), "{rendered}");
    }
}
