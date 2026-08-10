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
use crate::protocol::HostDisplay;

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
    /// `(enable_tls, enable_credssp)`, which is the shape both the config file
    /// and the RDP engine's own security selection are written in.
    pub fn flags(self) -> (bool, bool) {
        match self {
            Security::Auto => (true, true),
            Security::Nla => (false, true),
            Security::Tls => (true, false),
        }
    }
}

/// Remote-desktop protocol of a target. Each has a server-side engine feeding
/// the same browser protocol (docs/architecture.md): `rdp` via FreeRDP
/// (src/rdp.rs), `vnc` via the built-in RFB client (src/vnc.rs). A Mac is reached
/// with `subtype = "ard"`, Apple Screen Sharing Standard mode over RFB 3.8.
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
/// business; both of today's are `vnc`'s and describe the same Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Subtype {
    /// macOS Screen Sharing on the standard RFB 3.8 wire, authenticated the way
    /// Apple Remote Desktop does: the credentials are a *macOS account's* and the
    /// connection is named to the Mac, which is what makes it share the screen
    /// rather than a login window of its own (see [`crate::vnc`]). A third-party
    /// VNC server that happens to run on a Mac is not this — it is a plain `vnc`
    /// target.
    ///
    /// The Mac's metadata extension lists every attached display, permits selecting
    /// one or their combined desktop, and supplies each display's pixel density.
    /// Apple's native pasteboard is available. Once the first layout arrives, a
    /// second `SetEncodings` switches the rectangles from raw to zlib without losing
    /// that display metadata.
    Ard,
    /// The same Mac over Apple's own protocol revision, RFB 003.889: an
    /// AES-128-CBC record layer (see [`crate::vnc_record`]) carrying Apple's
    /// control messages (see [`crate::vnc_apple`]).
    ///
    /// **Experimental.** Alone among the subtypes, none of this is documented by
    /// Apple: the revision, its record layer, its control messages and its virtual
    /// display handling were all reverse engineered, and are only as correct as the
    /// Macs they have been measured against — docs/apple-vnc-889.md records which,
    /// and what is still inferred. A macOS update is free to change any of it, and
    /// the dynamic-resolution path remains reverse engineered.
    ///
    /// High Performance Screen Sharing uses a virtual display rather than the
    /// Mac's physical displays. This gateway requests one virtual display at the
    /// pinned [`TargetConfig::width`] and [`TargetConfig::height`] when both are
    /// set, or at the connecting client's screen resolution otherwise, carries
    /// zlib rectangles, and uses Apple's encrypted record transport.
    ///
    /// Apple's native pasteboard payloads are carried inside the encrypted record
    /// transport when `clipboard` is enabled. With `resize`, viewport reports
    /// replace the virtual display's one advertised mode and the Mac answers with
    /// its new layout. See docs/apple-vnc-889.md.
    ArdHighPerformance,
}

impl Subtype {
    /// The name as written in the config file.
    pub fn name(self) -> &'static str {
        match self {
            Subtype::Ard => "ard",
            Subtype::ArdHighPerformance => "ard-high-performance",
        }
    }

    /// Whether this subtype authenticates to a Mac the Apple Remote Desktop way
    /// (RFB security type 30), which both of them do and no plain `vnc` target
    /// does. What makes the credentials a macOS account's.
    pub fn apple_authentication(self) -> bool {
        match self {
            Subtype::Ard | Subtype::ArdHighPerformance => true,
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
/// Only implemented strategies are variants; anything else is refused by serde
/// with the list of what is accepted. See docs/architecture.md for the dial.
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
    /// The base encode, plus a second and much cheaper one for the cells changing
    /// fastest right now.
    ///
    /// Not a third way to encode every tile: it *builds on* the base a target
    /// would otherwise have, which is still what a settled cell is sent as. The
    /// base is read from [`RenderSubtype`] and [`TargetConfig::render_quality`]
    /// rather than from this axis, which `motion` occupies — `png` with no quality
    /// is a lossless base, a lossy subtype with a quality is a fixed-quality one.
    /// The moving encode has its own two keys,
    /// [`TargetConfig::render_motion_subtype`] and
    /// [`TargetConfig::render_motion_quality`], and it may be either a cheaper still
    /// per cell or — under [`MotionSubtype::Stream`] — a video stream per coalesced
    /// moving region.
    ///
    /// A cell that stops changing is re-sent once at the base encode, so a paused
    /// screen returns to full quality on its own: the base is the truth, motion is
    /// a temporary discount on what is too busy to notice.
    Motion,
    /// The whole desktop as one video stream, at a fixed quality
    /// ([`TargetConfig::render_quality`]).
    ///
    /// Not a codec on the [`RenderSubtype`] axis, and deliberately not: the other
    /// three are *per-tile* codecs, where every tile is independent, reorderable,
    /// cacheable and droppable once something covers it. An access unit is none of
    /// those — it is one link in a chain, and losing any link corrupts every frame
    /// after it until the next keyframe. So this axis is where it goes, and it
    /// refuses the subtype axis outright rather than pretending to be a fourth value
    /// on it.
    ///
    /// It follows that this is a different *transport*, not a different compressor:
    /// no tiles, no cell grid, no per-region decisions, one access unit per remote
    /// frame. VP9 carries it ([`crate::vp9`]) — so this axis names no codec either.
    Video,
}

/// What a target's redirected audio is carried as, chosen per target because it
/// is a bandwidth-against-processing trade and only the operator knows which side
/// of it a given link is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioCodec {
    /// Opus in 20 ms packets ([`crate::opus_stream`]), at
    /// [`TargetConfig::audio_bitrate`] (default 96 kbit/s). The default codec,
    /// and the right answer for any link that leaves the building: the default
    /// rate is well clear of where stereo Opus starts to be audibly lossy, and
    /// a fifteenth of what the alternative costs.
    #[default]
    Opus,
    /// The remote's own PCM, unencoded and unresampled ([`crate::pcm_stream`]):
    /// 1.41 Mbit/s, no encoder in the gateway and no decoder in the client.
    ///
    /// For a fast local network, where those megabits are free and the thing
    /// worth removing is everything that touches a sample: no encoder here, no
    /// resampler, and packets that reach the browser's output without passing
    /// through a decoder at all.
    Pcm,
}

impl AudioCodec {
    /// How the config key spells it, for messages that name it back.
    pub fn name(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Pcm => "pcm",
        }
    }
}

/// A target's audio keys as the encoder consumes them, resolved by
/// [`TargetConfig::audio_plan`]. In bits per second because that is libopus's
/// unit; the config speaks kbit/s because a person does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioPlan {
    pub codec: AudioCodec,
    /// The Opus bitrate — the ceiling, when the plan is adaptive. Carried but
    /// unread for [`AudioCodec::Pcm`], whose whole point is that no encoder
    /// exists to give it to.
    pub bitrate_bps: i32,
    /// `Some(floor)` exactly when the bitrate should track the audio socket's
    /// backpressure, walking between the floor and [`Self::bitrate_bps`] — and
    /// silence should be shed while the link is behind. See
    /// [`TargetConfig::audio_adaptive`].
    pub adaptive_floor_bps: Option<i32>,
}

impl AudioPlan {
    /// `codec` at the default rate, fixed — the plan a bare `audio_codec` key
    /// resolves to.
    pub fn fixed(codec: AudioCodec) -> Self {
        Self { codec, ..Self::default() }
    }
}

impl Default for AudioPlan {
    /// What an unset dial means: Opus at the default rate, fixed. The fallback
    /// [`crate::session`] uses when no target is selected, where there is no
    /// config to read.
    fn default() -> Self {
        Self {
            codec: AudioCodec::Opus,
            bitrate_bps: DEFAULT_AUDIO_BITRATE_KBPS as i32 * 1000,
            adaptive_floor_bps: None,
        }
    }
}

/// The codec a target's **base** tiles are encoded with — the second render axis,
/// paired with [`RenderType`]. Under `full` and `fixed-quality` that is every
/// tile; under [`RenderType::Motion`] it is every tile except the ones currently
/// in motion, which [`MotionSubtype`] names instead. All implemented codecs are
/// variants; serde refuses anything else.
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

/// The encode for what [`RenderType::Motion`] finds in motion — an axis of its own
/// rather than a reuse of [`RenderSubtype`], for two reasons.
///
/// The base is sent once when a cell settles and can afford WebP's slower, smaller
/// encode, while a moving cell is re-encoded every frame, where JPEG's faster
/// encode may beat WebP's smaller output; cheapest and smallest are not the same
/// question at quality 60 as at 10. And this is the axis `stream` appears on, where
/// the moving encode stops being a still image at all — which it could only do by
/// being nameable apart from the base. `png` is not a variant and never will be: a
/// moving cell needs a quality to turn down, and lossless has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MotionSubtype {
    /// Baseline JPEG at [`TargetConfig::render_motion_quality`].
    Jpeg,
    /// WebP at [`TargetConfig::render_motion_quality`].
    Webp,
    /// A video stream per coalesced moving region, at
    /// [`TargetConfig::render_motion_quality`], with the base codec carrying
    /// everything else.
    ///
    /// The other two are still pictures per cell, re-encoded from scratch every
    /// frame; this is an inter-frame stream, which is what moving content is cheap
    /// in. What it costs instead is statefulness — an access unit means nothing out
    /// of sequence — and that is why it never reaches the client as a tile. See
    /// [`crate::regions`] for which regions get a stream and when one ends, and
    /// [`crate::protocol::VideoUnit`] for what arrives.
    ///
    /// Never the default: it has to be written out, because unlike `jpeg` and `webp`
    /// it is not a cheaper version of the base but a different transport beside it.
    ///
    /// Named `stream` rather than after a codec because what it names is the transport;
    /// VP9 carries it ([`crate::vp9`]), the same way it carries `render_type = "video"`.
    Stream,
}

/// The tile encoder an engine uses, resolved from a target's render dial by
/// [`TargetConfig::render_plan`]. The axes and the qualities collapse to this, so
/// `rdp::run` / `vnc::run` and [`crate::encode::TileSink`] match on one value and
/// never touch the config enums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileCodec {
    /// Lossless PNG — the default path.
    Png,
    /// JPEG at the given quality (1–100).
    Jpeg(u8),
    /// WebP at the given quality (1–100).
    Webp(u8),
}

/// What the `motion` strategy does with what it finds moving, resolved from
/// [`MotionSubtype`] and [`TargetConfig::render_motion_quality`].
///
/// An enum rather than a codec and a flag, because the two arms are not two settings
/// of one mechanism: one produces an independent picture per cell, the other a link
/// in a chain per region. Everything that differs downstream — whether pixels may be
/// re-cut per cell, whether a record may be dropped, what a cleanup owes — follows
/// from which arm this is, and the compiler is what makes a consumer say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionEncode {
    /// A cheaper still per moving cell, at this codec's quality.
    Tile(TileCodec),
    /// A video stream per coalesced moving region, at this 1–100 quality.
    ///
    /// The quality is the dial rather than a quantizer: turning that into one is
    /// [`crate::vp9`]'s business, and it is the only module that should know what a
    /// quantizer is.
    Stream { quality: u8 },
}

/// The whole render dial as an engine sees it, and the one place the two ways this
/// gateway can put a desktop on a wire are told apart.
///
/// An enum rather than a struct with a flag, because the difference is not a setting:
/// [`Self::Tiles`] cuts damage into independent images, and [`Self::Video`] feeds one
/// stateful stream. Nothing sensible is shared between those two paths, and making it
/// an enum is what stops a consumer from quietly handling only the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPlan {
    /// One encoded image per changed region — every configuration that predates
    /// [`RenderPlan::Video`], and still the default.
    Tiles {
        /// What a settled cell is sent as, and what a cleanup pass restores a cell to.
        base: TileCodec,
        /// What a region changing fast is sent as instead, while it keeps changing.
        ///
        /// `None` is the switch that keeps the entire motion path off, and it is
        /// `None` for every configuration but `motion` — so a target that does not
        /// ask for the feature does not pay for it and is byte-identical to what it
        /// sent before the feature existed.
        motion: Option<MotionEncode>,
        /// Draw the motion strategy's decisions into the pixels. QA only, and only
        /// meaningful when `motion` is `Some`.
        debug: bool,
        /// The floor of the adaptive quality walk, when
        /// [`TargetConfig::render_adaptive`] asked for one; `None` keeps every
        /// quality exactly where the config put it. Applies to whatever lossy
        /// dials this plan has — a lossy base or motion tile codec takes a
        /// per-encode quality scaled with the link's lag, and a motion `stream`
        /// hands it to the same congestion walk `Video` uses.
        adaptive: Option<u8>,
    },
    /// The whole framebuffer as one video stream at a fixed quantizer.
    ///
    /// The quality is the 1–100 dial rather than a quantizer: turning that into one is
    /// [`crate::vp9`]'s business, and it is the only module that should know what a
    /// quantizer is.
    Video {
        quality: u8,
        /// The floor of the adaptive quality walk — see [`RenderPlan::Tiles`]'s
        /// field of the same name. `None` keeps the congestion walk's historical
        /// shape: pressure-only, floored at 1.
        adaptive: Option<u8>,
    },
}

impl RenderPlan {
    /// This plan in one line, for the client's session card.
    ///
    /// **The resolved plan rather than the config keys, and that is the point.** The dial has
    /// five keys with a pairing matrix between them, two of which default from a third, and
    /// what a target *does* is the plan they collapse to — so a description built from the
    /// keys would restate the file while the encoder did something the reader has to derive.
    /// This says what is running.
    ///
    /// It exists because that was invisible from a client: nothing on the wire said which of
    /// the seven combinations a session was on, so "why does this look soft" or "why is this
    /// target slower" began by reading the operator's config file — if the reader had it.
    /// Every combination the pairing matrix admits has a distinct rendering here, and
    /// `every_render_combination_describes_itself` is what keeps that true.
    pub fn describe(&self) -> String {
        fn tile(codec: TileCodec) -> String {
            match codec {
                TileCodec::Png => "lossless png".to_owned(),
                TileCodec::Jpeg(q) => format!("jpeg q{q}"),
                TileCodec::Webp(q) => format!("webp q{q}"),
            }
        }
        // The floor as a suffix, because it modifies the whole plan rather than
        // one dial: every quality named before it is a ceiling the link may fall
        // below, and this is how far.
        fn floor(adaptive: Option<u8>) -> String {
            adaptive.map_or_else(String::new, |floor| format!(" · adaptive ≥{floor}"))
        }
        match self {
            RenderPlan::Video { quality, adaptive } => {
                format!("video q{quality}{}", floor(*adaptive))
            }
            RenderPlan::Tiles { base, motion: None, adaptive, .. } => {
                // No motion arm at all, which is `full` and `fixed-quality` alike: the
                // difference between them is only whether the base is lossless, and that is
                // what the base already says.
                format!("tiles · {}{}", tile(*base), floor(*adaptive))
            }
            RenderPlan::Tiles { base, motion: Some(motion), debug, adaptive } => {
                let moving = match motion {
                    MotionEncode::Tile(codec) => tile(*codec),
                    MotionEncode::Stream { quality } => {
                        format!("stream q{quality}")
                    }
                };
                let debug = if *debug { " (debug outlines)" } else { "" };
                format!(
                    "motion · base {}, moving {moving}{debug}{}",
                    tile(*base),
                    floor(*adaptive)
                )
            }
        }
    }

}

/// One `[[targets]]` profile: a remote machine plus its credentials.
///
/// Credentials live here (server-side) and are used during the target protocol's
/// authentication handshake.
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
    /// more than one: `subtype = "ard"` on a `vnc` target is Apple Screen
    /// Sharing Standard mode. Unset means the protocol's ordinary form.
    ///
    /// Declared rather than sniffed from the credentials, because the two
    /// dialects want different ones and guessing which was meant is how a
    /// perfectly good password ends up authenticating nobody — see
    /// [`Subtype`]. Validated against the protocol in
    /// [`ConfigFile::parse`].
    #[serde(default)]
    pub subtype: Option<Subtype>,
    /// Target host.
    pub host: String,
    /// Target port. Omitted (or 0) means the protocol's standard port
    /// (3389 for RDP, 5900 for VNC) — normalized in [`ConfigFile::parse`].
    #[serde(default)]
    pub port: u16,
    /// Username. Required by RDP, and by a `vnc` target of either Apple subtype,
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
    /// takes. Rejected on other protocols and on either Apple [`Subtype`] — see
    /// [`ConfigFile::parse`].
    #[serde(default)]
    pub vnc_password: String,
    /// Optional domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// Pinned desktop width, in points. Optional, and *specified* means
    /// something: a target with a pinned size opens at it, while one without
    /// opens at the full resolution of the client's own screen — see
    /// [`Self::opening_size`]. Both keys come as a pair or not at all
    /// ([`ConfigFile::parse`]). Also the answer to
    /// [`crate::protocol::ClientMsg::DefaultSize`], a client with no
    /// desktop-shaped window of its own asking for whatever size this end
    /// considers right. A generic or Standard-mode VNC server keeps its own
    /// size at connect and is never asked.
    ///
    /// Points rather than pixels, because the density can move underneath it:
    /// an RDP connect happens at 1x and a Retina client then asks for twice
    /// the pixels, and `DefaultSize` has to keep meaning the same desktop
    /// rather than half of one. See `Density` in src/rdp.rs.
    #[serde(default)]
    pub width: Option<u16>,
    /// Pinned desktop height, in points. See [`Self::width`].
    #[serde(default)]
    pub height: Option<u16>,
    /// Security negotiation mode: `"auto"`, `"nla"`, or `"tls"`. RDP only —
    /// ignored for VNC targets (RFB security is negotiated per the handshake).
    #[serde(default)]
    pub security: Security,
    /// Allow client-driven resize: hand this target's desktop size to the
    /// client's window. A desktop client reports every window change while this
    /// is on; there is no client-side mode, manual resize command, or second
    /// config key.
    ///
    /// On RDP this also turns on density matching, because there a density *is* a
    /// resize: the Display Control channel this negotiates is the only way to tell
    /// a live session to render at 200%, so a Retina client gets twice the pixels
    /// and a UI drawn twice as large. Off, an RDP target ignores the client's
    /// density entirely.
    ///
    /// On `ard-high-performance` the setup descriptor always enables the Mac's
    /// dynamic geometry; this flag decides only whether the window keeps
    /// driving it after the open. Standard `ard` refuses the option because it
    /// exposes physical displays.
    #[serde(default)]
    pub resize: bool,
    /// RDP's graphics pipeline (EGFX), on by default and decoupled from
    /// [`Self::resize`]. With both on, a resize is a Display Control monitor
    /// layout under the pipeline — a graphics reset, no reactivation and no
    /// reconnect — which is what makes handing the size to the window cheap
    /// enough to do on every drag. The trade is a Windows host's text staying
    /// soft after an EGFX resize, where the legacy path's reactivation
    /// re-renders it sharp: set `egfx = false` to buy sharp text at the price
    /// of a reactivation per resize (and a reconnect where sound negotiated on
    /// the dynamic `rdpsnd` transport). `Option` rather than a bare default so
    /// that setting it on a VNC target, which has no graphics pipeline to
    /// switch, is refused at parse time instead of accepted and left inert;
    /// `None` reads as on ([`TargetConfig::egfx`]).
    #[serde(default)]
    pub egfx: Option<bool>,
    /// Clipboard bridge: let the browser read and write this target's
    /// clipboard, through the floating menu's Clipboard panel. Off by default —
    /// a remote desktop's clipboard often holds whatever was last copied there,
    /// so exposing it is a per-target decision rather than a default.
    ///
    /// Supported by both engines, though what reaches the far side differs:
    /// generic VNC uses the UTF-8 Extended Clipboard extension when available and
    /// falls back to latin-1 `ServerCutText`/`ClientCutText`; Apple VNC uses the
    /// native pasteboard protocol; RDP uses MS-RDPECLIP `CF_UNICODETEXT`.
    #[serde(default)]
    pub clipboard: bool,
    /// Negotiate RDP audio at connect. Packets are sent only while the attached
    /// client subscribes. Rejected for VNC.
    #[serde(default)]
    pub audio: bool,
    /// Which codec [`Self::audio`] encodes with; `None` reads as
    /// [`AudioCodec::Opus`]. `Option` rather than a bare default so that setting
    /// it on a target that never enabled audio is refused at parse time instead
    /// of accepted and left inert.
    #[serde(default)]
    pub audio_codec: Option<AudioCodec>,
    /// Opus bitrate in kbit/s (6–510); `None` reads as
    /// [`DEFAULT_AUDIO_BITRATE_KBPS`]. Opus only — passthrough PCM has no
    /// encoder to give a rate to, so the key is refused beside
    /// `audio_codec = "pcm"`.
    ///
    /// When [`Self::audio_adaptive`] is set this is the *ceiling*: the rate a
    /// link that keeps up gets, and the one the walk climbs back to.
    #[serde(default)]
    pub audio_bitrate: Option<u32>,
    /// Let the Opus bitrate track the audio socket's own backpressure: a send
    /// that blocks means the previous packets are still unwritten, and sustained
    /// blocking walks the bitrate down toward [`Self::audio_bitrate_min`]; a
    /// clear stretch walks it back up to the ceiling. While behind, wave buffers
    /// that are pure silence are shed instead of queued — silence is the one
    /// content whose loss is free, and dropping it is how the client catches up
    /// without a trimmed or resampled note anywhere (see [`crate::audio`]).
    ///
    /// Opus only, for the same reason as [`Self::audio_bitrate`].
    #[serde(default)]
    pub audio_adaptive: bool,
    /// Floor in kbit/s for [`Self::audio_adaptive`] (6–510, below the
    /// bitrate ceiling); `None` reads as [`DEFAULT_AUDIO_BITRATE_MIN_KBPS`].
    /// Requires `audio_adaptive` — a floor for a walk that never moves is a key
    /// that could not do anything.
    #[serde(default)]
    pub audio_bitrate_min: Option<u32>,
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
    ///
    /// Under [`RenderType::Motion`] this is the *base* quality — what a settled
    /// cell gets — and it is omitted when the base is lossless PNG.
    #[serde(default)]
    pub render_quality: Option<u8>,
    /// What [`RenderType::Motion`] does with what it finds moving. Defaults to
    /// whatever [`Self::render_subtype`] is, and is *required* when that is `png`,
    /// which has no quality to turn down. Refused for any other render type.
    ///
    /// `stream` is never a default: it is not a cheaper still but a video stream per
    /// moving region, so it has to be asked for.
    #[serde(default)]
    pub render_motion_subtype: Option<MotionSubtype>,
    /// Quality (1–100) for what is in motion — as cheap as it takes, since motion
    /// hides the artifacts and a region that stops moving is re-sent at the base
    /// encode anyway. Required by [`RenderType::Motion`] and refused for any other
    /// render type.
    #[serde(default)]
    pub render_motion_quality: Option<u8>,
    /// Outline every piece the motion strategy emits, in the pixels themselves, so
    /// what the detection decided is visible on the screen instead of inferred from
    /// how blurry something looks. A QA aid for [`RenderType::Motion`] and refused
    /// for any other render type; off unless asked for.
    ///
    /// See [`crate::encode::TileSink::damage`] for what the colours mean. The marks
    /// go on the *copy* handed to the encoder — for a region stream, on the crop it
    /// encodes rather than on the mirror — so the shadow, the stash and the mirror
    /// keep the true pixels, and a cleanup erases the outline it replaces.
    #[serde(default)]
    pub render_motion_debug: bool,
    /// Let quality track the measured link, on every lossy dial this target has.
    ///
    /// The configured qualities stay the *ceiling* — a link with room to spare
    /// never earns a better picture than the one asked for — and the walk's floor
    /// is [`Self::render_adaptive_min`]. What moves underneath:
    ///
    /// - A video stream (`render_type = "video"` or `render_motion_subtype =
    ///   "stream"`) already gives quality up when queueing a frame blocks; this
    ///   adds the client's own lag — how long the oldest unacknowledged paint
    ///   batch has been owed, beyond the link's measured floor — as a second
    ///   reason to, and moves the walk's floor up from 1.
    /// - A lossy tile codec (JPEG/WebP, base or motion) gets a quality per
    ///   *encode* instead of per session, scaled down linearly with that same
    ///   lag — Guacamole's curve, on this gateway's own signal.
    ///
    /// Refused for `render_type = "full"`, which is lossless everywhere and has
    /// no dial for this to move.
    #[serde(default)]
    pub render_adaptive: bool,
    /// Floor (1–100) for [`Self::render_adaptive`]; `None` reads as
    /// [`DEFAULT_RENDER_ADAPTIVE_MIN`]. Must not exceed any quality this target
    /// configures — a floor above the ceiling is a contradiction better refused
    /// than resolved. Requires `render_adaptive`.
    #[serde(default)]
    pub render_adaptive_min: Option<u8>,
}

/// The quality floor [`TargetConfig::render_adaptive`] falls back to when
/// [`TargetConfig::render_adaptive_min`] is unset. Low enough to matter on a
/// struggling link, high enough that text stays legible.
pub const DEFAULT_RENDER_ADAPTIVE_MIN: u8 = 20;

/// The Opus bitrate (kbit/s) when [`TargetConfig::audio_bitrate`] is unset —
/// [`crate::opus_stream`]'s long-standing default, well clear of where stereo
/// Opus starts to be audibly lossy.
pub const DEFAULT_AUDIO_BITRATE_KBPS: u32 = 96;

/// The adaptive floor (kbit/s) when [`TargetConfig::audio_bitrate_min`] is
/// unset. 32 kbit/s stereo Opus is degraded but continuous — and continuity is
/// the whole point of giving bitrate up.
pub const DEFAULT_AUDIO_BITRATE_MIN_KBPS: u32 = 32;

impl TargetConfig {
    /// The size a session opens at, in points: the explicitly configured
    /// `width`/`height` when the operator pinned one, else the full resolution
    /// of the client's own screen (named in
    /// [`crate::protocol::ClientMsg::Connect`]), else [`DEFAULT_SIZE`]. One
    /// rule for every engine that can ask for an opening size, so none of them
    /// branches on its own.
    pub fn opening_size(&self, display: Option<HostDisplay>) -> (u16, u16) {
        self.pinned_size()
            .or(display.map(|d| (d.w, d.h)))
            .unwrap_or(DEFAULT_SIZE)
    }

    /// The explicitly configured size, when the operator pinned one. Parse
    /// guarantees the keys come as a pair.
    pub fn pinned_size(&self) -> Option<(u16, u16)> {
        self.width.zip(self.height)
    }

    /// What [`crate::protocol::ClientMsg::DefaultSize`] restores: the pinned
    /// size, or the same default a sizeless session would have opened at.
    pub fn default_size(&self) -> (u16, u16) {
        self.pinned_size().unwrap_or(DEFAULT_SIZE)
    }

    /// RDP's graphics pipeline switch, on unless the operator traded it away.
    pub fn egfx(&self) -> bool {
        self.egfx.unwrap_or(true)
    }

    /// The tile encoders to use for this target. This is the whole of the render
    /// dial as the engines see it: the axes and the qualities collapse to one
    /// [`RenderPlan`], so `rdp::run` / `vnc::run` need not know the config enums.
    ///
    /// A lossy codec carries its quality, which [`ConfigFile::parse_with`] has
    /// already guaranteed is present and in range; each `None` arm falls back to
    /// the safe answer — lossless PNG for the base, no motion encode at all —
    /// rather than trusting that here.
    pub fn render_plan(&self) -> RenderPlan {
        let adaptive = self
            .render_adaptive
            .then(|| self.render_adaptive_min.unwrap_or(DEFAULT_RENDER_ADAPTIVE_MIN));
        if let (RenderType::Video, Some(quality)) = (self.render_type, self.render_quality) {
            return RenderPlan::Video { quality, adaptive };
        }
        let base = match (self.render_subtype, self.render_quality) {
            (RenderSubtype::Jpeg, Some(q)) => TileCodec::Jpeg(q),
            (RenderSubtype::Webp, Some(q)) => TileCodec::Webp(q),
            _ => TileCodec::Png,
        };
        let motion = match (self.render_type, self.render_motion_quality) {
            (RenderType::Motion, Some(q)) => match self.motion_subtype() {
                Some(MotionSubtype::Jpeg) => Some(MotionEncode::Tile(TileCodec::Jpeg(q))),
                Some(MotionSubtype::Webp) => Some(MotionEncode::Tile(TileCodec::Webp(q))),
                Some(MotionSubtype::Stream) => Some(MotionEncode::Stream { quality: q }),
                None => None,
            },
            _ => None,
        };
        RenderPlan::Tiles { base, motion, debug: self.render_motion_debug, adaptive }
    }

    /// The audio keys collapsed to what the encoder is built from, the same way
    /// [`Self::render_plan`] collapses the render dial: defaults resolved,
    /// kilobits turned into the bits libopus speaks, and the adaptive floor
    /// present exactly when the walk was asked for. Callers gate on
    /// [`Self::audio`] — a target without audio has no plan to resolve.
    pub fn audio_plan(&self) -> AudioPlan {
        let codec = self.audio_codec.unwrap_or_default();
        let bitrate_bps = self.audio_bitrate.unwrap_or(DEFAULT_AUDIO_BITRATE_KBPS) as i32 * 1000;
        let adaptive_floor_bps = (codec == AudioCodec::Opus && self.audio_adaptive).then(|| {
            self.audio_bitrate_min.unwrap_or(DEFAULT_AUDIO_BITRATE_MIN_KBPS) as i32 * 1000
        });
        AudioPlan { codec, bitrate_bps, adaptive_floor_bps }
    }

    /// Whether this target puts moving pixels on the wire as a video stream — either the whole
    /// desktop (`render_type = "video"`) or a region at a time (`render_motion_subtype =
    /// "stream"`).
    ///
    /// Answered off the render dial alone, without resolving a plan, because
    /// [`ConfigFile::parse_with`] asks it before it has validated the qualities a plan needs.
    pub fn streams_video(&self) -> bool {
        match self.render_type {
            RenderType::Video => true,
            RenderType::Motion => self.motion_subtype() == Some(MotionSubtype::Stream),
            RenderType::Full | RenderType::FixedQuality => false,
        }
    }

    /// The motion encode this target asked for, falling back to the base codec when
    /// the key is omitted — which `stream` never is, since a stream is not a cheaper
    /// version of a still. `None` only for the pairing parse rejects: a `png` base
    /// with no motion subtype named.
    fn motion_subtype(&self) -> Option<MotionSubtype> {
        self.render_motion_subtype.or(match self.render_subtype {
            RenderSubtype::Jpeg => Some(MotionSubtype::Jpeg),
            RenderSubtype::Webp => Some(MotionSubtype::Webp),
            RenderSubtype::Png => None,
        })
    }
}

/// What a session opens at when neither the config nor the connecting client
/// named a size: no screen to measure, no operator to ask, one desk-shaped
/// answer.
///
/// Points, not backing pixels, and 16:10 because a remote desktop is a working
/// surface rather than a video. It is also what a phone gets: a touch client
/// asks for this rather than its own screen, which is portrait and far too
/// small to be a desktop (`sendMobileSize` in `frontend/src/useRemoteDesktop.ts`).
pub const DEFAULT_SIZE: (u16, u16) = (1920, 1200);

/// Where a served gateway listens when nothing says otherwise.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:52380";

/// The prefix that picks a Unix socket instead of a TCP address.
pub const UNIX_LISTEN_PREFIX: &str = "unix:";

/// What a gateway listens on: a TCP address, or a Unix socket.
///
/// A socket is for a gateway that only ever answers a reverse proxy on the same
/// machine — nginx, Caddy, a systemd unit — where a loopback port is a port every
/// other local process can reach and a socket is a file the filesystem can guard.
/// It is not an option for the browser, which cannot address one: the client
/// reaches its gateway over HTTP and two WebSockets, and both need a host and a
/// port. Whatever terminates that proxy is what a browser talks to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddr {
    /// `host:port`, with any IPv6 literal bracketed — resolvable by
    /// [`std::net::ToSocketAddrs`] as it stands.
    Tcp(String),
    /// The path of a Unix socket to create.
    Unix(PathBuf),
}

impl std::fmt::Display for ListenAddr {
    /// The way it is written in the config, so a log line can be pasted back into
    /// one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(addr) => f.write_str(addr),
            Self::Unix(path) => write!(f, "{UNIX_LISTEN_PREFIX}{}", path.display()),
        }
    }
}

/// The optional `[server]` block: web-server bind and frontend location.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    /// Address the web server binds to: `host:port`, or `unix:<path>` for a
    /// socket a reverse proxy connects to (default [`DEFAULT_LISTEN`]).
    ///
    /// One key rather than two because it is one decision:
    /// a host without a port and a port without a host are each half an answer,
    /// and `--listen`/`REMOTEX_LISTEN` can only override the whole of it —
    /// overriding one half from the command line and taking the other from the
    /// file is how a gateway ends up on an address nobody wrote down.
    pub listen: Option<String>,
    /// Directory holding the built frontend; overrides [`default_static_dir`].
    pub static_dir: Option<PathBuf>,
    /// Web-login credential: `username:bcrypt_hash`, generated with
    /// `remotex gen-passwd <username>`. Required — without a login everything
    /// but the SPA shell and `/api/auth/*` refuses requests, so an empty
    /// value would lock the server to nobody.
    pub site_passwd: Option<String>,
    // No `branding` here: it is the top-level `[branding]` table now (see
    // `ConfigFile::branding`), because `remotex.app`'s config has no `[server]`
    // block to hold it and one value with two spellings is one of them going
    // stale. `deny_unknown_fields` refuses a file that still has it here.
    /// **Development only.** A label to give this gateway its own hostname on
    /// loopback: a browser arriving at `127.0.0.1`, `::1` or `localhost` is
    /// redirected to `<label>.remotex.localhost`, keeping the port and path.
    ///
    /// It exists for one problem, which has no other clean answer: a cookie is
    /// scoped by *host* and ignores the port, so two gateways on one machine
    /// share `remotex_session` and each login silently evicts the other. The
    /// gateway you were not touching then answers 401 to everything, and its
    /// browser drops to the login screen the next time anything asks — which
    /// reads as a session bug in whatever you were actually testing. Testing
    /// session takeover needs two gateways, so this is not a rare corner.
    ///
    /// Under `.localhost` because every name below it resolves to loopback without
    /// DNS (RFC 6761) and is a *distinct* cookie origin, so two gateways become two
    /// independent logins in one browser. Under `.remotex.localhost` in particular
    /// so the names this project hands out are all one suffix, taken from nobody.
    ///
    /// Never reachable in a deployment: [`AppConfig::dev_hostname`] redirects only
    /// a request whose own `Host` is a loopback name, so a gateway behind a real
    /// hostname or address ignores this however it is set.
    pub dev_subdomain: Option<String>,
}

/// The default display name when `[branding].text` is unset.
pub const DEFAULT_BRANDING: &str = "remotex";

/// The `[branding]` table as written: what the deployment calls itself, and the
/// image it puts in the browser tab.
///
/// Top-level rather than in `[server]`, and it is the **only** place to set it.
/// `remotex.app`'s config has no `[server]` block at all ([`Audience::Embedded`]),
/// so a table that lived there could not name the app — and accepting both
/// spellings would be two places to write one value, with the loser losing
/// silently. `deny_unknown_fields` refuses a file that still has anything of it
/// under `[server]`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrandingSection {
    /// Display name of this gateway: the browser's login screen, interstitials and
    /// tab title, and in `remotex.app` the heading above its target list, its window
    /// title and its launch screen.
    ///
    /// Defaults to [`DEFAULT_BRANDING`]; whitespace-only is treated as absent.
    pub text: Option<String>,
    /// Path to an image file the gateway serves as the page's icon (`GET
    /// /api/logo`, the favicon of every client tab). The content type comes from
    /// the extension, so an extension nothing recognizes as an image is refused
    /// at resolution — see [`logo_mime`]. Unset means the page keeps no icon,
    /// exactly as before.
    pub logo: Option<PathBuf>,
}

/// The resolved branding: always a name, and an icon when one was configured.
#[derive(Clone, Debug)]
pub struct Branding {
    /// Display name for the login screen, interstitials, and browser tab title.
    pub text: String,
    /// The icon file, with its content type already decided.
    pub logo: Option<Logo>,
}

/// A configured logo file, paired with the content type it is served under.
///
/// The pair exists so the one place that knows extension → MIME ([`logo_mime`])
/// runs at config resolution — a gateway never serves an icon it could not name,
/// and `check-config` refuses the file before it is saved.
#[derive(Clone, Debug)]
pub struct Logo {
    /// As written in the config; a relative path resolves against the process's
    /// working directory, the same as `[server].static_dir`.
    pub path: PathBuf,
    pub mime: &'static str,
}

/// The content type `[branding].logo` is served under, from its extension.
///
/// A closed list rather than a guess: what belongs here is what browsers take as
/// a favicon, and an extension outside it is far more likely a typo than a format
/// this list forgot.
fn logo_mime(path: &Path) -> anyhow::Result<&'static str> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => Ok("image/png"),
        Some("ico") => Ok("image/x-icon"),
        Some("svg") => Ok("image/svg+xml"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("gif") => Ok("image/gif"),
        Some("webp") => Ok("image/webp"),
        _ => anyhow::bail!(
            "[branding].logo {} is not an image a browser tab can show — \
             use .png, .ico, .svg, .jpg, .gif or .webp",
            path.display()
        ),
    }
}

/// Who a config file is for, and therefore which rules it is held to.
///
/// The difference is not cosmetic — each audience makes a demand the other one
/// cannot meet — which is why this is a parameter of parsing rather than something
/// checked later by whoever happens to remember to:
///
/// - a [`Self::Served`] gateway is useless without a target to offer and a
///   credential to guard it, and it is told where to listen;
/// - an [`Self::Embedded`] one is started by `remotex.app` with the port, the
///   secret and the web root decided by the app, so a `[server]` block could only
///   contradict it — and it must come up with **no targets at all**, because that
///   is what a first launch has and the picker's job is to say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// `remotex serve`: a browser's gateway.
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
    /// The `[branding]` table: display name and tab icon. See [`BrandingSection`]
    /// for why it is top-level and nowhere else. A config that still writes the
    /// old `branding = "…"` string fails to parse — a table is not a string, and
    /// that refusal is the whole of the migration.
    #[serde(default)]
    pub branding: Option<BrandingSection>,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

/// Resolved runtime configuration: the web server plus every target profile it
/// serves (the browser picks one after login).
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Where the web server binds, already validated by `parse_listen`.
    ///
    /// A TCP port of `0` asks the kernel for an ephemeral one, which is what an
    /// embedded gateway does — the port it got is then read off the listener and
    /// told to its client, never guessed.
    pub listen: ListenAddr,
    /// Directory holding the built frontend (index.html + assets), served from
    /// disk. Defaults to [`default_static_dir`] for a served gateway; an embedded
    /// one is told where its bundle keeps it (`--web-root`).
    ///
    /// Every gateway has one, because every client is the same SPA: a browser
    /// loads it over the network, while `remotex.app` maps the same directory to
    /// `remotex://app` and the embedded gateway serves it over loopback beside
    /// that custom-scheme view.
    pub static_dir: PathBuf,
    /// Every target profile this process serves; the post-login picker selects
    /// one. Non-empty for [`Audience::Served`]; possibly empty for an embedded
    /// gateway, whose client shows "no targets are configured" instead.
    pub targets: Vec<TargetConfig>,
    /// What gets a request past the door: a login, or the embedded client's token.
    pub auth: GatewayAuth,
    /// The deployment's name and, when configured, the icon file behind
    /// `GET /api/logo`.
    pub branding: Branding,
    /// `<label>.remotex.localhost` to send a loopback browser to, from
    /// `[server].dev_subdomain`. `None` disables the redirect entirely.
    ///
    /// Stored as the whole hostname rather than the label so the one place that
    /// validated it is the only place that builds it — a redirect target
    /// assembled at the point of use is one that can be assembled wrongly.
    pub dev_hostname: Option<String>,
    /// Answer cross-origin requests from the **shell's** origin —
    /// `remotex://app`, the custom scheme `remotex.app` loads its client from —
    /// with credentials allowed.
    ///
    /// True for [`Audience::Embedded`] and false everywhere else, and the
    /// difference is not a preference. `remotex.app` loads its client from a
    /// `remotex://` scheme its main process registers, so every call the page
    /// makes to its gateway is cross-origin; without this the page cannot reach
    /// the backend it was shipped with.
    ///
    /// On a **served** gateway the same header would be a hole with nothing behind
    /// it: that gateway is reachable by browsers on a network and has a login cookie
    /// worth stealing, and no browser on a network can be a `remotex://` document
    /// anyway, so answering for one is a header that can only ever be wrong. An
    /// embedded gateway is bound to loopback, serves the single client that started
    /// it, and its credential is a token minted per launch and kept in that app's own
    /// cookie store — so the audience that gets this header is the audience for which
    /// it grants nothing anybody else can use.
    pub allow_shell_origin: bool,
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
            // socket, a web root the app hands over on the command line
            // (`serve-embedded --web-root`), and a token instead of a login. A key
            // that is quietly overridden is worse than
            // one that is refused: it reads as configuration and behaves as
            // decoration.
            anyhow::ensure!(
                config.server.is_none(),
                "this config is remotex.app's own and may not have a [server] block: \
                 the app decides where its gateway listens, where the client it \
                 serves comes from, and how it authenticates. Only [branding] and \
                 [[targets]] belong here"
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
            // A pinned size is a pair. One key alone is not half a pin — it is a
            // config that would silently open at a size the operator half-chose.
            anyhow::ensure!(
                target.width.is_some() == target.height.is_some(),
                "target {:?} sets {} without {} — a pinned size needs both, and leaving both \
                 out opens the session at the client screen's own resolution",
                target.name,
                if target.width.is_some() { "width" } else { "height" },
                if target.width.is_some() { "height" } else { "width" }
            );
            // And a pin of nothing is not a pin: a zero axis would ask every
            // engine for a desktop that cannot exist.
            anyhow::ensure!(
                target.pinned_size().is_none_or(|(w, h)| w > 0 && h > 0),
                "target {:?} pins a {:?}×{:?} size, but width and height must both be \
                 greater than zero",
                target.name,
                target.width,
                target.height
            );
            // Audio is RDP's alone, and refused elsewhere rather than ignored.
            // MS-RDPEA is the one audio channel this gateway speaks; RFB has no
            // equivalent at all, so `audio = true` on a VNC target could only
            // ever be a mistake about what the protocol carries. Naming that at
            // parse time is the difference between a config error and a session
            // that is silent for no stated reason.
            //
            // Everything downstream of the channel — the socket, the bridge, the
            // encoders — is protocol-agnostic, which is why this rule is about
            // the *engine* and not about any of them.
            // The graphics pipeline is RDP's alone, the same way: EGFX is an RDP
            // channel, so on a VNC target the key could only be a belief about
            // the wrong protocol, and either value would be silently inert.
            anyhow::ensure!(
                target.egfx.is_none() || target.protocol == Protocol::Rdp,
                "target {:?} sets egfx on a {} target, and only rdp has a graphics pipeline \
                 to switch. Remove the key.",
                target.name,
                target.protocol.name()
            );
            anyhow::ensure!(
                !target.audio || target.protocol == Protocol::Rdp,
                "target {:?} sets audio on a {} target, and only rdp carries it: MS-RDPEA is \
                 an RDP channel and RFB has no equivalent. Remove the key to start the \
                 session without sound.",
                target.name,
                target.protocol.name()
            );
            // Same rule one step down: a codec for audio that was never turned on
            // is a key that could not do anything, and the likely typo behind it
            // is a forgotten `audio = true` rather than a deliberate choice.
            anyhow::ensure!(
                target.audio_codec.is_none() || target.audio,
                "target {:?} sets audio_codec but not audio, so nothing would encode",
                target.name
            );
            // The bitrate keys and the adaptive switch are Opus's alone: passthrough
            // PCM has no encoder, so a rate beside it is a key that could not do
            // anything — same rule as audio_codec without audio, one step down again.
            let opus = target.audio && target.audio_codec.unwrap_or_default() == AudioCodec::Opus;
            anyhow::ensure!(
                target.audio_bitrate.is_none() || opus,
                "target {:?} sets audio_bitrate, which only an opus audio target uses — it \
                 is the encoder's rate, and this target has no opus encoder",
                target.name
            );
            anyhow::ensure!(
                !target.audio_adaptive || opus,
                "target {:?} sets audio_adaptive, which only an opus audio target uses — \
                 adapting means moving the encoder's bitrate, and this target has no opus \
                 encoder",
                target.name
            );
            anyhow::ensure!(
                target.audio_bitrate_min.is_none() || target.audio_adaptive,
                "target {:?} sets audio_bitrate_min but not audio_adaptive — the floor \
                 belongs to the adaptive walk, and without the walk nothing would read it",
                target.name
            );
            let bitrate = target.audio_bitrate.unwrap_or(DEFAULT_AUDIO_BITRATE_KBPS);
            if let Some(kbps) = target.audio_bitrate {
                anyhow::ensure!(
                    (6..=510).contains(&kbps),
                    "target {:?} sets audio_bitrate = {kbps}, which is out of range — it is \
                     in kbit/s and must be 6–510",
                    target.name
                );
            }
            if let Some(kbps) = target.audio_bitrate_min {
                anyhow::ensure!(
                    (6..=510).contains(&kbps),
                    "target {:?} sets audio_bitrate_min = {kbps}, which is out of range — it \
                     is in kbit/s and must be 6–510",
                    target.name
                );
                anyhow::ensure!(
                    kbps < bitrate,
                    "target {:?} sets audio_bitrate_min = {kbps} at or above the bitrate \
                     ceiling of {bitrate} kbit/s, which leaves the adaptive walk nowhere \
                     to go",
                    target.name
                );
            }
            // Which credentials a VNC target may carry is the subtype's to say,
            // and the two sets do not overlap: an Apple subtype authenticates an
            // account to a Mac, plain VncAuth proves a secret the machine holds.
            // Mixing them is how a password ends up authenticating nobody, so each
            // is refused where it cannot be used rather than quietly ignored.
            match (target.protocol, target.subtype) {
                (Protocol::Vnc, Some(subtype @ (Subtype::Ard | Subtype::ArdHighPerformance))) => {
                    let name = subtype.name();
                    anyhow::ensure!(
                        !target.username.is_empty() && !target.password.is_empty(),
                        "target {:?} is subtype {name:?} but has no username and password — \
                         both are needed, and on a Mac they are an account's there",
                        target.name
                    );
                    anyhow::ensure!(
                        target.vnc_password.is_empty(),
                        "target {:?} is subtype {name:?} but sets vnc_password, which only a \
                         plain \"vnc\" target uses — Apple's authentication carries the \
                         account credentials above instead",
                        target.name
                    );
                    // Standard mode shares the Mac's physical displays and has no
                    // virtual display for a viewport to resize. High Performance
                    // owns one, and may replace its configured mode dynamically.
                    anyhow::ensure!(
                        subtype != Subtype::Ard || !target.resize,
                        "target {:?} is subtype {name:?} and sets resize, which this gateway \
                         does not support: Standard Screen Sharing exposes the Mac's physical \
                         displays, whose resolution this gateway does not change",
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
            // The motion keys belong to one strategy, and a config that sets them
            // anywhere else has misunderstood which dial it is turning — more
            // likely a `render_type` that was never changed than a deliberate
            // choice, so it is worth saying so rather than silently ignoring them.
            if target.render_type != RenderType::Motion {
                anyhow::ensure!(
                    target.render_motion_subtype.is_none()
                        && target.render_motion_quality.is_none()
                        && !target.render_motion_debug,
                    "target {:?} sets render_motion_subtype, render_motion_quality or \
                     render_motion_debug without render_type = \"motion\" — those keys \
                     describe the cheaper encode \"motion\" gives the cells that are changing \
                     fastest, and no other strategy has one",
                    target.name
                );
            }
            // Refused rather than merely discouraged, because the failure is not
            // one a person recovers from by looking at the screen: a resize under
            // both leaves the desktop wrong until the whole gateway is restarted,
            // and a reconnect does not clear it. Neither half is proven at fault —
            // `ard-high-performance` is reverse engineered with no specification
            // behind it (docs/apple-vnc-889.md), and `motion` is the newer code —
            // so the pairing waits until one of them is understood well enough to
            // say which. Every other subtype may use `motion`, and this target may
            // use every other strategy.
            anyhow::ensure!(
                target.render_type != RenderType::Motion
                    || target.subtype != Some(Subtype::ArdHighPerformance),
                "target {:?} pairs render_type = \"motion\" with subtype = \
                 \"ard-high-performance\", which this gateway refuses: a resize under both \
                 corrupts the desktop until the gateway is restarted. Use subtype = \"ard\" \
                 to keep \"motion\", or another render_type to keep the virtual display",
                target.name
            );
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
                // `motion` reads the base off the subtype and the quality rather
                // than off `render_type`, which it occupies itself: a `png` base
                // is lossless and takes no quality, a lossy base needs one. This
                // is the only strategy that can express a lossless base with a
                // lossy discount, which is the interesting one — text and flat UI
                // stay perfect and only what moves gets ugly.
                (RenderType::Motion, RenderSubtype::Png) => {
                    anyhow::ensure!(
                        target.render_quality.is_none(),
                        "target {:?} pairs render_type \"motion\" with render_subtype \"png\" \
                         and sets render_quality, which the lossless base has no use for. \
                         render_motion_quality is the dial for the cells in motion; drop \
                         render_quality, or set a lossy render_subtype to give the base a \
                         quality too",
                        target.name
                    );
                    anyhow::ensure!(
                        target.render_motion_subtype.is_some(),
                        "target {:?} pairs render_type \"motion\" with render_subtype \"png\" \
                         but names no render_motion_subtype. The motion encode defaults to the \
                         base codec, and PNG is lossless — it has no quality to turn down — so \
                         a PNG base must name its own: \"jpeg\", \"webp\", or \"stream\" for a \
                         video stream per moving region",
                        target.name
                    );
                }
                (RenderType::Motion, RenderSubtype::Jpeg | RenderSubtype::Webp) => {
                    let q = target.render_quality.with_context(|| format!(
                        "target {:?} is render_type \"motion\" with a lossy render_subtype, \
                         which makes that subtype the *base* encode — the one a settled cell \
                         gets — so it needs a render_quality, an integer 1–100. Use \
                         render_subtype = \"png\" for a lossless base",
                        target.name
                    ))?;
                    anyhow::ensure!(
                        (1..=100).contains(&q),
                        "target {:?} sets render_quality = {q}, which is out of range — it \
                         must be 1–100",
                        target.name
                    );
                }
                // `video` is the one strategy with nothing on the subtype axis to
                // pair with. The other three cut damage into independent images and
                // choose which codec to encode them with; this one is a single
                // stateful video stream carrying the whole framebuffer, so there is
                // no per-tile codec left to name.
                (RenderType::Video, RenderSubtype::Png) => {
                    let q = target.render_quality.with_context(|| format!(
                        "target {:?} is render_type \"video\" but sets no render_quality — it \
                         needs one, an integer 1–100. It is the quality the stream holds on a \
                         link that can carry it; a link that cannot will fall below it",
                        target.name
                    ))?;
                    anyhow::ensure!(
                        (1..=100).contains(&q),
                        "target {:?} sets render_quality = {q}, which is out of range — it \
                         must be 1–100",
                        target.name
                    );
                }
                (RenderType::Video, RenderSubtype::Jpeg | RenderSubtype::Webp) => anyhow::bail!(
                    "target {:?} sets render_type \"video\" with render_subtype {:?}. \
                     render_subtype names a codec for each changed region separately, and \
                     \"video\" does not send regions at all — it sends the whole desktop as one \
                     video stream, where every frame depends on the one before it. Drop \
                     render_subtype to keep \"video\", or set \
                     render_type = \"fixed-quality\" to keep this one",
                    target.name,
                    match target.render_subtype {
                        RenderSubtype::Jpeg => "jpeg",
                        _ => "webp",
                    }
                ),
            }
            // Both `motion` pairings need this, and neither of the arms above is
            // the place for it: the moving encode is the whole point of the
            // strategy, and it is the one key that has no default to fall back on.
            if target.render_type == RenderType::Motion {
                let q = target.render_motion_quality.with_context(|| format!(
                    "target {:?} is render_type \"motion\" but sets no \
                     render_motion_quality — it needs one, an integer 1–100. It is what the \
                     cells in motion are encoded at, and can go as low as it takes: motion \
                     hides the artifacts, and a cell that stops moving is re-sent at the base \
                     encode. Under render_motion_subtype = \"stream\" it is the quality each \
                     region's stream holds on a link that can carry it, and a link that \
                     cannot will fall below it",
                    target.name
                ))?;
                anyhow::ensure!(
                    (1..=100).contains(&q),
                    "target {:?} sets render_motion_quality = {q}, which is out of range — \
                     it must be 1–100",
                    target.name
                );
            }
            // The adaptive switch needs a dial to move. Every strategy has one
            // except `full`, which is lossless everywhere — and a `motion` plan
            // always has at least the motion quality, so only `full` can be empty.
            anyhow::ensure!(
                !target.render_adaptive || target.render_type != RenderType::Full,
                "target {:?} sets render_adaptive with render_type \"full\", which is \
                 lossless everywhere and has no quality for the link to move. Pick a \
                 strategy with a lossy dial — \"fixed-quality\", \"motion\" or \"video\"",
                target.name
            );
            anyhow::ensure!(
                target.render_adaptive_min.is_none() || target.render_adaptive,
                "target {:?} sets render_adaptive_min but not render_adaptive — the floor \
                 belongs to the adaptive walk, and without the walk nothing would read it",
                target.name
            );
            if let Some(floor) = target.render_adaptive_min {
                anyhow::ensure!(
                    (1..=100).contains(&floor),
                    "target {:?} sets render_adaptive_min = {floor}, which is out of range — \
                     it must be 1–100",
                    target.name
                );
                // A floor above a ceiling is a contradiction, and every configured
                // quality is a ceiling the walk must fit under.
                let ceiling = target.render_quality.into_iter()
                    .chain(target.render_motion_quality)
                    .min();
                if let Some(ceiling) = ceiling {
                    anyhow::ensure!(
                        floor <= ceiling,
                        "target {:?} sets render_adaptive_min = {floor} above a configured \
                         quality of {ceiling}, which leaves the adaptive walk nowhere to go",
                        target.name
                    );
                }
            }
        }
        Ok(config)
    }

    /// Resolve the runtime configuration of the gateway inside `remotex.app`:
    /// loopback, an ephemeral port, the SPA out of the app's bundle, and a freshly
    /// minted token.
    ///
    /// Every one of those is an argument here rather than a default that
    /// `[server]` could override, which is what [`Audience::Embedded`] enforces on
    /// the way in. `[branding]` is the one thing such a config *may* say about the
    /// gateway itself, because it is about the app rather than about the server: it
    /// names a window, not a deployment, and two instances on one Mac are easier to
    /// tell apart if they can be called different things.
    pub fn resolve_embedded(
        self,
        token: EmbeddedToken,
        web_root: PathBuf,
    ) -> anyhow::Result<AppConfig> {
        Ok(AppConfig {
            // Not `localhost`: that name resolves to both loopbacks and the client
            // is told one port on one address. The app connects to 127.0.0.1, on
            // whatever port the kernel gives us.
            //
            // TCP rather than a socket, and not by omission: the thing that talks to
            // this gateway is a page in a window, and a page addresses its gateway
            // with URLs — an HTTP origin and two WebSockets. None of that can name a
            // socket file. See [`ListenAddr`].
            listen: ListenAddr::Tcp("127.0.0.1:0".to_owned()),
            static_dir: web_root,
            targets: self.targets,
            auth: GatewayAuth::Token(token),
            branding: Self::resolve_branding(self.branding.as_ref())?,
            dev_hostname: None,
            // The client is loaded as `remotex://app` out of the bundle, so it talks
            // to this gateway cross-origin. See the field.
            allow_shell_origin: true,
        })
    }

    /// The `[branding]` table resolved: the display name (or
    /// [`DEFAULT_BRANDING`]), and the logo with its content type decided.
    ///
    /// Whitespace-only text counts as absent: a heading of one space is not a name
    /// somebody meant to give. Shared by both audiences because it is one table now,
    /// and a second copy of these rules is how the two would come to differ. Failing
    /// here is what puts a bad logo extension in front of `check-config` — both
    /// audiences resolve on the way through it.
    fn resolve_branding(configured: Option<&BrandingSection>) -> anyhow::Result<Branding> {
        let section = configured.cloned().unwrap_or_default();
        Ok(Branding {
            text: section
                .text
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_BRANDING)
                .to_owned(),
            logo: section
                .logo
                .map(|path| anyhow::Ok(Logo { mime: logo_mime(&path)?, path }))
                .transpose()?,
        })
    }

    /// Resolve the runtime configuration with the file's own listen address.
    /// See [`Self::resolve_with`] for the overriding form.
    pub fn resolve(self) -> anyhow::Result<AppConfig> {
        self.resolve_with(None)
    }

    /// Resolve the runtime configuration: validate the web-login credential and
    /// carry over every target profile (the browser picks one after login).
    ///
    /// `listen` is `--listen`/`REMOTEX_LISTEN` when either was given, and it wins
    /// over `[server].listen`. That is the whole precedence: one address, from the
    /// command line if it is there and from the file otherwise.
    pub fn resolve_with(self, listen: Option<&str>) -> anyhow::Result<AppConfig> {
        let server = self.server.unwrap_or_default();
        let listen = match (listen, server.listen.as_deref()) {
            (Some(value), _) => parse_listen(value).context("invalid --listen address")?,
            (None, Some(value)) => parse_listen(value).context("invalid [server].listen")?,
            (None, None) => ListenAddr::Tcp(DEFAULT_LISTEN.to_owned()),
        };
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
            listen,
            static_dir: server.static_dir.unwrap_or_else(default_static_dir),
            // Non-empty is guaranteed by `parse`.
            targets: self.targets,
            auth: GatewayAuth::Login(site_passwd),
            branding: Self::resolve_branding(self.branding.as_ref())?,
            dev_hostname: server
                .dev_subdomain
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(dev_hostname)
                .transpose()
                .context("invalid [server].dev_subdomain")?,
            // Never on a gateway browsers reach over a network. See the field.
            allow_shell_origin: false,
        })
    }
}

/// Validate a listen address and return it in the form the bind path uses.
///
/// `unix:<path>` is taken as written, path and all: everything after the prefix is
/// the socket's path, including anything that looks like a port, because a file
/// name is not parsed for one.
///
/// Otherwise it is TCP, and a port is required rather than defaulted: this value is
/// written in exactly one place now, so `0.0.0.0` on its own is far more likely to
/// be somebody who thinks they also said which port than somebody asking for 52380.
/// `0` is a legitimate port here — it is how the kernel is asked for an ephemeral
/// one.
///
/// An IPv6 literal must be bracketed, because without brackets there is nothing to
/// tell `::1` from `<host>:<port>`: `::1` alone would be read as host `::` on port
/// 1, which is a plausible address and the wrong one. Rather than guess, the
/// unbracketed form is refused and the message says so.
fn parse_listen(value: &str) -> anyhow::Result<ListenAddr> {
    let value = value.trim();
    if let Some(path) = value.strip_prefix(UNIX_LISTEN_PREFIX) {
        anyhow::ensure!(
            !path.is_empty(),
            "{UNIX_LISTEN_PREFIX} names no socket — write the path out, as in \
             \"{UNIX_LISTEN_PREFIX}/run/remotex/gateway.sock\""
        );
        return Ok(ListenAddr::Unix(PathBuf::from(path)));
    }
    let (host, port) = value.rsplit_once(':').with_context(|| {
        format!("{value:?} is not host:port — the port is required, as in \"{DEFAULT_LISTEN}\"")
    })?;
    anyhow::ensure!(
        !host.is_empty(),
        "{value:?} names no host — write the interface out, as in \"0.0.0.0:{port}\""
    );
    let port: u16 = port
        .parse()
        .with_context(|| format!("{port:?} is not a port number (0-65535)"))?;
    // Brackets as well as colons: `[localhost]:52380` has no colon in its host and
    // would otherwise pass here, to fail at `lookup_host` on the way up instead —
    // which is a config mistake reported as a resolver one. A bracket is only ever
    // an IPv6 literal's, so anything wearing them has to be one.
    if host.contains([':', '[', ']']) {
        let literal = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .with_context(|| {
                format!(
                    "a host with a colon or brackets must be a bracketed IPv6 \
                     address, as in \"[::1]:{port}\""
                )
            })?;
        literal
            .parse::<std::net::Ipv6Addr>()
            .with_context(|| format!("{literal:?} is not an IPv6 address"))?;
    }
    Ok(ListenAddr::Tcp(format!("{host}:{port}")))
}

/// `<label>.remotex.localhost`, refusing anything that is not a single DNS label.
///
/// The check is what makes the redirect target unforgeable: a `Location` built
/// from an unvalidated string could name any host at all, and this one is
/// assembled from a label that has been proved to contain no dot, no slash, no
/// colon and no credentials. So the target is always some name under
/// `.localhost`, which by RFC 6761 can only be loopback.
///
/// The `remotex` label in the middle is what keeps a development gateway from
/// claiming a name somebody else's tooling may already answer to: `gw-a.localhost`
/// is a name anything on this machine may have taken, while everything under
/// `.remotex.localhost` is this project's by construction. It is one name to
/// recognise in a browser's history and one suffix to clear cookies for.
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
         and no dots (it is used as <label>.remotex.localhost)"
    );
    anyhow::ensure!(
        !label.starts_with('-') && !label.ends_with('-'),
        "{label:?} may not start or end with a hyphen"
    );
    Ok(format!("{label}.remotex.localhost"))
}

/// Load the config file: the explicit `--config` path, or the global path of the
/// installed layout. Returns the parsed file and the path it came from.
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
            "no --config given and not running from an installed layout — \
             pass --config <path>",
        ),
    }
}

/// The one global config location for the running installation.
pub fn installed_config_path() -> Option<PathBuf> {
    Some(installed_layout()?.config)
}

/// Paths belonging to one recognized installation.
struct InstalledLayout {
    config: PathBuf,
    static_dir: PathBuf,
}

/// Resolve the package-manager layout or the quick installer's relocatable
/// layout from the executable that is actually running.
fn installed_layout() -> Option<InstalledLayout> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    installed_layout_for_exe(&exe)
}

fn installed_layout_for_exe(exe: &Path) -> Option<InstalledLayout> {
    let bin_dir = exe.parent()?;

    // Native Linux packages own the executable and web bundle at their FHS
    // paths. Configuration is administrator-created under /etc, not under
    // /usr: package removal must not delete a file containing credentials.
    if bin_dir == Path::new("/usr/bin") {
        return Some(InstalledLayout {
            config: "/etc/remotex/remotex.toml".into(),
            static_dir: "/usr/share/remotex/web".into(),
        });
    }

    // The macOS package is the same direct layout under the locally managed
    // prefix. Its configuration follows that prefix as well.
    if bin_dir == Path::new("/usr/local/bin") {
        return Some(InstalledLayout {
            config: "/usr/local/etc/remotex/remotex.toml".into(),
            static_dir: "/usr/local/share/remotex/web".into(),
        });
    }

    // The fallback quick installer is relocatable. Its launcher resolves to
    // <prefix>/versions/<version>/bin/remotex, while configuration deliberately
    // lives outside the version being replaced.
    let version_root = bin_dir.parent()?;
    let versions_dir = version_root.parent()?;
    if versions_dir.file_name()? != "versions" {
        return None;
    }
    Some(InstalledLayout {
        config: versions_dir.parent()?.join("etc/remotex.toml"),
        static_dir: version_root.join("share/remotex/web"),
    })
}

/// Say so, before binding, when the web root is not one.
///
/// The SPA handler still answers per request, so this changes nothing about what
/// happens — it changes whether anyone can tell *why*. A gateway with no page to
/// serve is a browser tab showing a 404, or `remotex.app` showing a blank window,
/// and neither says which of the two ends is wrong.
///
/// `hint` is the half that differs: a served gateway is told where to look in its
/// config, and an embedded one is told this path came from its own bundle — the
/// config it reads has no key for it and `[server]` is refused there.
pub fn warn_if_no_web_root(static_dir: &Path, hint: &str) {
    if !static_dir.is_dir() {
        log::warn!(
            "static dir {} not found — the web UI will 404 ({hint})",
            static_dir.display()
        );
    } else if !static_dir.join("index.html").is_file() {
        log::warn!(
            "no index.html in static dir {} — the web UI will 404 ({hint})",
            static_dir.display()
        );
    }
}

/// Default location of the built frontend.
///
/// Prefers the web bundle belonging to a recognized installation; falls back
/// to `frontend/dist` relative to the working directory for `cargo run` in a
/// checkout. Override with `static_dir` in the `[server]` block.
pub fn default_static_dir() -> PathBuf {
    if let Some(layout) = installed_layout()
        && layout.static_dir.is_dir()
    {
        return layout.static_dir;
    }
    PathBuf::from("frontend/dist")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_paths_follow_each_install_layout() {
        let linux = installed_layout_for_exe(Path::new("/usr/bin/remotex")).unwrap();
        assert_eq!(linux.config, Path::new("/etc/remotex/remotex.toml"));
        assert_eq!(linux.static_dir, Path::new("/usr/share/remotex/web"));

        let mac = installed_layout_for_exe(Path::new("/usr/local/bin/remotex")).unwrap();
        assert_eq!(mac.config, Path::new("/usr/local/etc/remotex/remotex.toml"));
        assert_eq!(mac.static_dir, Path::new("/usr/local/share/remotex/web"));

        let quick = installed_layout_for_exe(Path::new(
            "/srv/remotex/versions/0.0.144/bin/remotex",
        ))
        .unwrap();
        assert_eq!(quick.config, Path::new("/srv/remotex/etc/remotex.toml"));
        assert_eq!(
            quick.static_dir,
            Path::new("/srv/remotex/versions/0.0.144/share/remotex/web")
        );

        assert!(installed_layout_for_exe(Path::new("/checkout/target/debug/remotex")).is_none());
    }

    /// The moving encode a plan resolves, for the tests that are about that and not
    /// about which arm of [`RenderPlan`] they landed in. `video` has none by
    /// construction — there are no cells to find in motion.
    fn motion_of(plan: RenderPlan) -> Option<MotionEncode> {
        match plan {
            RenderPlan::Tiles { motion, .. } => motion,
            RenderPlan::Video { .. } => None,
        }
    }

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
        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
        let GatewayAuth::Login(site_passwd) = &config.auth else {
            panic!("a served gateway logs in");
        };
        assert_eq!(site_passwd.username(), "admin");
        assert_eq!(config.targets.len(), 1);
        let t = &config.targets[0];
        assert_eq!(t.name, "one");
        assert_eq!(t.protocol, Protocol::Rdp);
        assert_eq!((t.host.as_str(), t.port), ("192.0.2.10", 3389));
        assert_eq!(t.pinned_size(), None, "an unpinned size follows the client's screen");
        assert_eq!(t.default_size(), DEFAULT_SIZE);
        assert_eq!(t.security, Security::Auto);
        assert!(t.username.is_empty() && t.password.is_empty() && t.domain.is_none());
        assert!(!t.resize, "dynamic resize is opt-in");
        assert!(t.egfx(), "the graphics pipeline is on unless the operator trades it away");
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

    /// One key, and it carries both halves — including the shapes the old pair
    /// could not express as one string, which is what the bracket rule is for.
    #[test]
    fn a_listen_address_is_one_key() {
        assert_eq!(resolved(r#"listen = "0.0.0.0:8080""#).listen.to_string(), "0.0.0.0:8080");
        assert_eq!(resolved(r#"listen = "[::1]:52380""#).listen.to_string(), "[::1]:52380");
        assert_eq!(
            resolved(r#"listen = "localhost:52380""#).listen.to_string(),
            "localhost:52380"
        );
        // Trimmed: a stray space cannot become part of an address.
        assert_eq!(resolved(r#"listen = "  127.0.0.1:1  ""#).listen.to_string(), "127.0.0.1:1");
        // Port 0 is how the kernel is asked for an ephemeral one.
        assert_eq!(resolved(r#"listen = "127.0.0.1:0""#).listen.to_string(), "127.0.0.1:0");
        // The pair this replaced is gone, not accepted alongside it.
        for gone in [r#"host = "0.0.0.0""#, "port = 8080"] {
            assert!(
                ConfigFile::parse(&with_server(gone)).is_err(),
                "{gone} is not half of [server].listen"
            );
        }
    }

    /// The other kind of address, for a gateway that answers a reverse proxy on
    /// the same machine instead of a browser directly.
    #[test]
    fn a_unix_socket_is_a_listen_address_too() {
        assert_eq!(
            resolved(r#"listen = "unix:/run/remotex/gateway.sock""#).listen,
            ListenAddr::Unix(PathBuf::from("/run/remotex/gateway.sock"))
        );
        // Everything after the prefix is the path — a file name is not parsed for
        // a port, however much of one it looks like.
        assert_eq!(
            resolved(r#"listen = "unix:/tmp/gw:52380.sock""#).listen,
            ListenAddr::Unix(PathBuf::from("/tmp/gw:52380.sock"))
        );
        // Relative is allowed: it is a path, and a service's working directory is
        // its own business.
        assert_eq!(
            resolved(r#"listen = "unix:gateway.sock""#).listen,
            ListenAddr::Unix(PathBuf::from("gateway.sock"))
        );
        // It round-trips through the display form, which is what the log prints.
        assert_eq!(
            resolved(r#"listen = "unix:/run/gw.sock""#).listen.to_string(),
            "unix:/run/gw.sock"
        );
        // A prefix and nothing else names no socket.
        let err = ConfigFile::parse(&with_server(r#"listen = "unix:""#))
            .and_then(ConfigFile::resolve)
            .expect_err("a prefix is not a path");
        assert!(format!("{err:#}").contains("[server].listen"), "{err:#}");
    }

    #[test]
    fn a_listen_address_that_is_not_host_port_is_refused() {
        for bad in [
            // Half an address. A defaulted port here would silently serve
            // something other than what was written.
            "0.0.0.0",
            "localhost",
            // Unbracketed IPv6: `::1` reads as host `::` on port 1, which is a
            // plausible address and the wrong one, so it is refused rather than
            // guessed at.
            "::1",
            "::1:52380",
            "[::1:52380",
            "[::zz]:52380",
            // Brackets are an IPv6 literal's and nothing else's, so a name or an
            // IPv4 address wearing them is a mistake to catch here rather than at
            // the resolver.
            "[localhost]:52380",
            "[127.0.0.1]:52380",
            ":52380",
            "127.0.0.1:",
            "127.0.0.1:notaport",
            "127.0.0.1:65536",
            "127.0.0.1:-1",
        ] {
            let err = ConfigFile::parse(&with_server(&format!("listen = {bad:?}")))
                .and_then(ConfigFile::resolve)
                .expect_err("should be refused: {bad:?}");
            assert!(
                format!("{err:#}").contains("[server].listen"),
                "{bad:?} was refused without naming the key: {err:#}"
            );
        }
    }

    /// `--listen`/`REMOTEX_LISTEN` replaces the file's address whole, and is held
    /// to the same shape — an override nobody validated is the one that turns a
    /// typo into a gateway on an address nothing reaches.
    #[test]
    fn the_command_line_listen_address_wins_and_is_checked() {
        let file = ConfigFile::parse(&with_server(r#"listen = "127.0.0.1:1""#)).unwrap();
        assert_eq!(
            file.clone().resolve_with(Some("0.0.0.0:8080")).unwrap().listen.to_string(),
            "0.0.0.0:8080"
        );
        // Absent, the file still decides.
        assert_eq!(
            file.clone().resolve_with(None).unwrap().listen.to_string(),
            "127.0.0.1:1"
        );
        // And a config with no address at all falls back to the default.
        assert_eq!(
            ConfigFile::parse(&minimal())
                .unwrap()
                .resolve_with(None)
                .unwrap()
                .listen
                .to_string(),
            DEFAULT_LISTEN
        );

        let err = file.resolve_with(Some("0.0.0.0")).unwrap_err();
        assert!(
            format!("{err:#}").contains("--listen"),
            "a bad override must name where it came from: {err:#}"
        );
    }

    // The dev-only hostname. Its validation is the reason the redirect target is
    // unforgeable: a `Location` is built from this and nothing else, so a value
    // carrying a dot, a slash, a colon or credentials would point somewhere that is
    // not loopback at all.
    #[test]
    fn a_dev_subdomain_becomes_one_label_under_remotex_localhost() {
        assert_eq!(
            resolved(r#"dev_subdomain = "a""#).dev_hostname.as_deref(),
            Some("a.remotex.localhost")
        );
        // Unset, and whitespace-only, both disable it — as `branding` does.
        assert_eq!(resolved("").dev_hostname, None);
        assert_eq!(resolved(r#"dev_subdomain = "  ""#).dev_hostname, None);
        // Trimmed, so a stray space cannot become part of a hostname.
        assert_eq!(
            resolved(r#"dev_subdomain = "  b  ""#).dev_hostname.as_deref(),
            Some("b.remotex.localhost")
        );
        // Digits and inner hyphens are legal in a DNS label.
        assert_eq!(
            resolved(r#"dev_subdomain = "gw-2""#).dev_hostname.as_deref(),
            Some("gw-2.remotex.localhost")
        );
    }

    #[test]
    fn a_dev_subdomain_that_is_not_one_label_is_refused() {
        for bad in [
            // A dot would move the name out from under `.remotex.localhost`
            // entirely, which is the whole of what keeps the target on loopback.
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
            Some(&*format!("{}.remotex.localhost", "a".repeat(63)))
        );
    }

    #[test]
    fn branding_defaults_and_overrides() {
        // Unset → the default name, no logo.
        let config = ConfigFile::parse(&minimal()).unwrap().resolve().unwrap();
        assert_eq!(config.branding.text, DEFAULT_BRANDING);
        assert!(config.branding.logo.is_none());

        // Set → carried through, trimmed. A top-level table, which is the only
        // place it lives: an app instance's config has no [server] block to hold it.
        let toml = format!(
            r#"
            [branding]
            text = "  Acme Remote  "
            logo = "/etc/remotex/acme.png"

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
        assert_eq!(config.branding.text, "Acme Remote");
        let logo = config.branding.logo.expect("the logo was configured");
        assert_eq!(logo.path, PathBuf::from("/etc/remotex/acme.png"));
        assert_eq!(logo.mime, "image/png");

        // Whitespace-only → falls back to the default.
        let toml = toml.replace("  Acme Remote  ", "   ");
        let config = ConfigFile::parse(&toml).unwrap().resolve().unwrap();
        assert_eq!(config.branding.text, DEFAULT_BRANDING);
    }

    /// The old top-level string spelling is gone, and gone loudly: a table is not
    /// a string, so the file fails to parse rather than quietly naming nothing.
    #[test]
    fn the_old_branding_string_is_refused() {
        let toml = format!("branding = \"remotex\"\n{}", minimal());
        let err = ConfigFile::parse(&toml).expect_err("a string is not a [branding] table");
        assert!(format!("{err:#}").contains("branding"), "{err:#}");
    }

    /// The logo's content type is decided at resolution, so a file no browser
    /// would take as an icon is refused before a gateway ever serves it —
    /// including by `check-config`, which resolves on the way through.
    #[test]
    fn a_logo_that_is_not_an_image_is_refused() {
        for bad in ["logo = \"/etc/remotex/logo.pdf\"", "logo = \"/etc/remotex/logo\""] {
            let toml = format!("[branding]\n{bad}\n{}", minimal());
            let err = ConfigFile::parse(&toml)
                .and_then(ConfigFile::resolve)
                .expect_err("not a favicon format");
            assert!(format!("{err:#}").contains("[branding].logo"), "{err:#}");
        }
        // Case does not decide it: .PNG is the same file format.
        let toml = format!("[branding]\nlogo = \"C:/logo.PNG\"\n{}", minimal());
        let config = ConfigFile::parse(&toml).unwrap().resolve().unwrap();
        assert_eq!(config.branding.logo.unwrap().mime, "image/png");
    }

    #[test]
    fn full_config_parses() {
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            listen = "0.0.0.0:8080"
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
        assert_eq!(config.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.static_dir, PathBuf::from("/srv/web"));
        // Every profile is carried over, in file order, for the picker.
        assert_eq!(config.targets.len(), 2);
        let win = &config.targets[0];
        assert_eq!(win.name, "win");
        assert_eq!(win.security, Security::Nla);
        assert_eq!(win.domain.as_deref(), Some("CORP"));
        assert_eq!(win.pinned_size(), Some((1920, 1080)));
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
        assert_eq!(
            t.render_plan(),
            RenderPlan::Tiles { base: TileCodec::Png, motion: None, debug: false, adaptive: None }
        );
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
        assert_eq!(
            t.render_plan(),
            RenderPlan::Tiles { base: TileCodec::Jpeg(60), motion: None, debug: false, adaptive: None }
        );
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
        assert_eq!(
            t.render_plan(),
            RenderPlan::Tiles { base: TileCodec::Webp(50), motion: None, debug: false, adaptive: None }
        );
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
    fn video_is_accepted_with_a_quality() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "video"
            render_quality = 60
            "#,
        )
        .expect("video with a quality");
        assert_eq!(cfg.targets[0].render_plan(), RenderPlan::Video { quality: 60, adaptive: None });
    }

    #[test]
    fn video_without_a_quality_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "video"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_quality"), "{err:#}");
    }

    #[test]
    fn a_video_quality_out_of_range_is_rejected() {
        for q in ["0", "101"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                render_type = "video"
                render_quality = {q}
                "#
            );
            let err = ConfigFile::parse(&toml).unwrap_err();
            assert!(format!("{err:#}").contains("1–100"), "q={q}: {err:#}");
        }
    }

    /// The refusal that says what `video` is: a codec per changed region is a
    /// different idea from one stream for the whole desktop, and naming one on a
    /// video target means somebody expected the wrong thing to happen.
    #[test]
    fn video_refuses_a_render_subtype() {
        for subtype in ["jpeg", "webp"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                render_type = "video"
                render_subtype = "{subtype}"
                render_quality = 60
                "#
            );
            let err = ConfigFile::parse(&toml).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(subtype), "the message should name the subtype: {msg}");
            assert!(msg.contains("video stream"), "the message should say what video is: {msg}");
            assert!(msg.contains("fixed-quality"), "the message should say the way out: {msg}");
        }
    }

    /// The motion keys belong to `motion`, and `video` is not a second place to put
    /// them — its stream has no cells to find in motion. Covered by the guard every
    /// non-motion strategy shares, and asserted here because `video` is the newest
    /// strategy and the one most likely to be tried with them.
    #[test]
    fn video_refuses_the_motion_keys() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "video"
            render_quality = 60
            render_motion_quality = 10
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_motion_quality"), "{err:#}");
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
        assert!(
            msg.contains("full") && msg.contains("fixed-quality") && msg.contains("motion"),
            "{msg}"
        );
    }

    // ---- the motion strategy ----

    /// The configuration the fixed dial cannot express at all, and the one the
    /// whole scheme is for: text and flat UI stay perfect, and only what moves
    /// gets ugly.
    #[test]
    fn motion_over_a_lossless_base_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_motion_subtype = "jpeg"
            render_motion_quality = 10
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_type, RenderType::Motion);
        assert_eq!(t.render_subtype, RenderSubtype::Png);
        assert_eq!(
            t.render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Png,
                motion: Some(MotionEncode::Tile(TileCodec::Jpeg(10))),
                debug: false,
                adaptive: None
            }
        );
    }

    /// A lossy base keeps its own meaning — `render_subtype` and `render_quality`
    /// are what a settled cell gets — and the motion codec defaults to it, so only
    /// the quality has to be named twice.
    #[test]
    fn motion_over_a_lossy_base_defaults_its_codec_to_the_base() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_subtype = "webp"
            render_quality = 60
            render_motion_quality = 10
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Webp(60),
                motion: Some(MotionEncode::Tile(TileCodec::Webp(10))),
                debug: false,
                adaptive: None
            }
        );
    }

    /// The reason the motion codec is an axis of its own: a moving cell is
    /// re-encoded every frame, where JPEG's faster encode may beat WebP's smaller
    /// output, so it need not be the codec a settled cell gets.
    #[test]
    fn the_motion_codec_need_not_be_the_base_codec() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_subtype = "webp"
            render_quality = 60
            render_motion_subtype = "jpeg"
            render_motion_quality = 10
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Webp(60),
                motion: Some(MotionEncode::Tile(TileCodec::Jpeg(10))),
                debug: false,
                adaptive: None
            }
        );
    }

    /// Every combination the pairing matrix admits, described distinctly.
    ///
    /// Built from real config files rather than from `RenderPlan` literals, so this also
    /// pins the resolution: what a reader sees in the session card is what these keys
    /// actually collapse to, including the two that default from `render_subtype`.
    ///
    /// Distinctness is asserted as a set, because the failure this guards against is not a
    /// wrong string but *two combinations reading the same* — which is the one way a
    /// debugging aid can send somebody looking in the wrong place.
    #[test]
    fn every_render_combination_describes_itself() {
        let cases = [
            ("full, the default", "render_type = \"full\"", "tiles · lossless png"),
            (
                "fixed quality jpeg",
                "render_type = \"fixed-quality\"\nrender_subtype = \"jpeg\"\nrender_quality = 60",
                "tiles · jpeg q60",
            ),
            (
                "fixed quality webp",
                "render_type = \"fixed-quality\"\nrender_subtype = \"webp\"\nrender_quality = 55",
                "tiles · webp q55",
            ),
            (
                "motion over a lossless base",
                "render_type = \"motion\"\nrender_motion_subtype = \"jpeg\"\nrender_motion_quality = 30",
                "motion · base lossless png, moving jpeg q30",
            ),
            (
                "motion whose moving encode defaults to the base",
                "render_type = \"motion\"\nrender_subtype = \"webp\"\nrender_quality = 70\nrender_motion_quality = 35",
                "motion · base webp q70, moving webp q35",
            ),
            (
                "motion with a stream per region",
                "render_type = \"motion\"\nrender_subtype = \"jpeg\"\nrender_quality = 70\nrender_motion_subtype = \"stream\"\nrender_motion_quality = 40",
                "motion · base jpeg q70, moving stream q40",
            ),
            (
                "the debug outlines, which are a different session to be looking at",
                "render_type = \"motion\"\nrender_motion_subtype = \"jpeg\"\nrender_motion_quality = 30\nrender_motion_debug = true",
                "motion · base lossless png, moving jpeg q30 (debug outlines)",
            ),
            (
                "the whole desktop as one stream",
                "render_type = \"video\"\nrender_quality = 60",
                "video q60",
            ),
        ];

        let mut seen: Vec<String> = Vec::new();
        for (what, keys, expected) in cases {
            let toml = format!(
                "[server]\n{}\n\n[[targets]]\nname = \"t\"\nprotocol = \"rdp\"\n\
                 host = \"192.0.2.10\"\n{keys}\n",
                site_passwd_line()
            );
            let cfg = ConfigFile::parse(&toml)
                .unwrap_or_else(|e| panic!("{what} should be a legal dial: {e:#}"));
            let described = cfg.targets[0].render_plan().describe();
            assert_eq!(described, expected, "{what}");
            seen.push(described);
        }

        let mut distinct = seen.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            seen.len(),
            "two render combinations describe themselves the same way: {seen:?}"
        );
    }

    /// The flagship pairing for a stream per moving region: a lossless base, so the
    /// text beside a video is exact and never re-encoded, and the stream carrying only
    /// what moves.
    #[test]
    fn a_stream_per_moving_region_over_a_lossless_base_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_motion_subtype = "stream"
            render_motion_quality = 30
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Png,
                motion: Some(MotionEncode::Stream { quality: 30 }),
                debug: false,
                adaptive: None
            }
        );
    }

    /// A stream is not a cheaper still, so it is never what a lossy base falls back
    /// to — it has to be asked for by name.
    #[test]
    fn a_stream_is_never_the_default_motion_encode() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_subtype = "webp"
            render_quality = 60
            render_motion_quality = 30
            "#,
        )
        .unwrap();
        assert_eq!(
            motion_of(cfg.targets[0].render_plan()),
            Some(MotionEncode::Tile(TileCodec::Webp(30))),
            "a lossy base defaulted its moving encode to a stream"
        );
    }

    /// The dial the stream holds to has no default either — the same requirement
    /// the still motion encodes have, for the same reason.
    #[test]
    fn a_stream_without_a_motion_quality_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_motion_subtype = "stream"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_motion_quality"), "{err:#}");
    }

    /// PNG has no quality to turn down, so the default the lossy bases enjoy is
    /// not available and the key has to be named.
    #[test]
    fn a_lossless_base_must_name_its_motion_codec() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_motion_quality = 10
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_motion_subtype"), "{err:#}");
    }

    /// The moving encode is the whole point of the strategy and has no default.
    #[test]
    fn motion_without_a_motion_quality_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_motion_subtype = "jpeg"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_motion_quality"), "{err:#}");
    }

    #[test]
    fn a_motion_quality_out_of_range_is_rejected() {
        for q in ["0", "101"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                render_type = "motion"
                render_motion_subtype = "jpeg"
                render_motion_quality = {q}
                "#
            );
            let err = ConfigFile::parse(&toml).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("render_motion_quality"), "q={q}: {msg}");
            assert!(msg.contains("1–100"), "q={q}: {msg}");
        }
    }

    /// A lossy base under `motion` still needs its own quality: the subtype names
    /// what a *settled* cell is encoded as, and that is not the motion quality.
    #[test]
    fn a_lossy_motion_base_still_needs_its_own_quality() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_subtype = "jpeg"
            render_motion_quality = 10
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_quality"), "{err:#}");
    }

    /// The QA overlay rides on the motion strategy and is off unless asked for, so
    /// a target that never turns it on cannot be paying for it by accident.
    #[test]
    fn the_motion_debug_overlay_is_opt_in_and_belongs_to_motion() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_motion_subtype = "jpeg"
            render_motion_quality = 10
            render_motion_debug = true
            "#,
        )
        .unwrap();
        assert!(matches!(cfg.targets[0].render_plan(), RenderPlan::Tiles { debug: true, .. }));

        let plain = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            "#,
        )
        .unwrap();
        assert!(
            matches!(plain.targets[0].render_plan(), RenderPlan::Tiles { debug: false, .. }),
            "the overlay defaulted on"
        );

        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_motion_debug = true
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_motion_debug"), "{err:#}");
    }

    /// A lossless base takes no quality, exactly as under `full`.
    #[test]
    fn render_quality_on_a_lossless_motion_base_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_quality = 60
            render_motion_subtype = "jpeg"
            render_motion_quality = 10
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("lossless base"), "{err:#}");
    }

    /// More likely a `render_type` that was never changed than a deliberate
    /// choice, so it is worth saying so rather than ignoring the keys.
    #[test]
    fn the_motion_keys_are_refused_by_every_other_strategy() {
        for extra in [
            "render_motion_subtype = \"jpeg\"",
            "render_motion_quality = 10",
        ] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                render_type = "fixed-quality"
                render_subtype = "jpeg"
                render_quality = 60
                {extra}
                "#
            );
            let err = ConfigFile::parse(&toml).unwrap_err();
            assert!(format!("{err:#}").contains("\"motion\""), "{extra}: {err:#}");
        }
    }

    /// `png` is not a motion codec and never will be — a moving cell needs a
    /// quality to turn down, and lossless has none. The error says what is legal
    /// on the axis rather than pointing back at the base codecs.
    #[test]
    fn png_is_not_a_motion_codec() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_motion_subtype = "png"
            render_motion_quality = 10
            "#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("jpeg") && msg.contains("webp"), "{msg}");
    }

    /// A resize under both corrupts the desktop until the gateway is restarted,
    /// which a person cannot recover from by looking at the screen — so the pairing
    /// is refused at load rather than left to be discovered. The error names both
    /// ways out, because either half alone is fine.
    #[test]
    fn motion_is_refused_on_apple_high_performance() {
        let target = |subtype: &str| {
            format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "vnc"
                subtype = "{subtype}"
                host = "h"
                username = "u"
                password = "p"
                width = 1600
                height = 1000
                render_type = "motion"
                render_motion_subtype = "jpeg"
                render_motion_quality = 10
                "#
            )
        };

        let err = ConfigFile::parse(&target("ard-high-performance")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ard-high-performance") && msg.contains("motion"), "{msg}");
        assert!(msg.contains("restarted"), "the error should say what goes wrong: {msg}");

        // The other Apple subtype is untouched: this is one pairing, not a rule
        // about Macs.
        let cfg = ConfigFile::parse(&target("ard")).expect("motion belongs on plain ard");
        assert_eq!(
            motion_of(cfg.targets[0].render_plan()),
            Some(MotionEncode::Tile(TileCodec::Jpeg(10)))
        );
    }

    /// And the same target is fine on any other strategy — what is refused is the
    /// pairing, not the subtype.
    #[test]
    fn apple_high_performance_keeps_every_other_render_type() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "vnc"
            subtype = "ard-high-performance"
            host = "h"
            username = "u"
            password = "p"
            width = 1600
            height = 1000
            render_type = "fixed-quality"
            render_subtype = "webp"
            render_quality = 60
            "#,
        )
        .expect("only the motion pairing is refused");
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles { base: TileCodec::Webp(60), motion: None, debug: false, adaptive: None }
        );
    }

    /// The switch that keeps the whole motion path off: nothing but `motion`
    /// resolves a moving encode, so every configuration that shipped before it
    /// existed still encodes every tile the one way.
    #[test]
    fn no_other_strategy_resolves_a_motion_encode() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"

            [[targets]]
            name = "b"
            protocol = "rdp"
            host = "h"
            render_type = "fixed-quality"
            render_subtype = "jpeg"
            render_quality = 60
            "#,
        )
        .unwrap();
        for t in &cfg.targets {
            assert_eq!(motion_of(t.render_plan()), None, "target {:?}", t.name);
        }
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

        // Standard mode exposes physical displays, which this gateway never resizes.
        let err =
            ard("username = \"andrew\"\npassword = \"h\"\nresize = true").unwrap_err();
        assert!(format!("{err:#}").contains("does not support"), "{err:#}");

        // Both Apple subtypes use Apple's native pasteboard messages.
        assert!(ard("username = \"andrew\"\npassword = \"h\"\nclipboard = true").is_ok());

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

    /// The high-performance subtype carries the same account credentials and native
    /// Apple pasteboard as plain `ard`, and requests a virtual display at
    /// width/height.
    #[test]
    fn the_high_performance_subtype_accepts_clipboard_and_resize() {
        let hp = |extra: &str| {
            ConfigFile::parse(&vnc_toml(&format!(
                "subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"\n{extra}"
            )))
        };

        let target = &hp("width = 1600\nheight = 1000\nresize = true\nclipboard = true")
            .unwrap()
            .targets[0];
        assert_eq!(target.subtype, Some(Subtype::ArdHighPerformance));
        assert_eq!(target.pinned_size(), Some((1600, 1000)));
        assert!(target.resize);
        assert!(target.clipboard);
        // The name is what a config file writes, hyphens and all — the enum is
        // kebab-case, not lowercase, and this is what pins that.
        assert_eq!(target.subtype.unwrap().name(), "ard-high-performance");

        // The credential rules are the ones `ard` has, shared rather than restated.
        let err = ConfigFile::parse(&vnc_toml(
            "subtype = \"ard-high-performance\"\nvnc_password = \"other\"",
        ))
        .unwrap_err();
        assert!(format!("{err:#}").contains("no username and password"), "{err:#}");
    }

    /// The opening size resolves the same way for every engine: a pinned size
    /// beats the client's screen, the screen beats the built-in default, and a
    /// single width without its height is refused rather than half-obeyed.
    #[test]
    fn the_opening_size_prefers_pinned_then_screen_then_default() {
        let screen = HostDisplay { w: 1728, h: 1117, scale: 200 };

        let pinned = &ConfigFile::parse(&vnc_toml("width = 1600\nheight = 1000")).unwrap().targets[0];
        assert_eq!(pinned.opening_size(Some(screen)), (1600, 1000));
        assert_eq!(pinned.default_size(), (1600, 1000));

        let free = &ConfigFile::parse(&vnc_toml("")).unwrap().targets[0];
        assert_eq!(free.opening_size(Some(screen)), (1728, 1117));
        assert_eq!(free.opening_size(None), DEFAULT_SIZE);
        assert_eq!(free.default_size(), DEFAULT_SIZE);

        let err = ConfigFile::parse(&vnc_toml("width = 1600")).unwrap_err();
        assert!(
            format!("{err:#}").contains("sets width without height"),
            "{err:#}"
        );
    }

    /// A zero axis is refused on every target alike — a High Performance
    /// virtual display was merely the first place it was caught misbehaving.
    #[test]
    fn a_pinned_size_requires_nonzero_dimensions() {
        for dimensions in ["width = 0\nheight = 1000", "width = 1600\nheight = 0"] {
            for subtype in ["", "subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"\n"] {
                let err = ConfigFile::parse(&vnc_toml(&format!("{subtype}{dimensions}")))
                    .unwrap_err();
                assert!(
                    format!("{err:#}").contains("width and height must both be greater than zero"),
                    "{err:#}"
                );
            }
        }
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

        // Clipboard is accepted for both engines, including RDP via MS-RDPECLIP.
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

    /// EGFX is RDP's, and refused on VNC by name — either value, since a key
    /// that could not do anything is a config error, not a preference.
    #[test]
    fn egfx_belongs_to_rdp_and_is_refused_on_vnc() {
        for value in ["true", "false"] {
            let err = ConfigFile::parse(&format!(
                r#"
                [server]
                {}

                [[targets]]
                name = "nope"
                protocol = "vnc"
                host = "10.0.0.5"
                egfx = {value}
                "#,
                site_passwd_line()
            ))
            .unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains("egfx"), "{rendered}");
            assert!(rendered.contains("rdp"), "the protocol that has it is named: {rendered}");
        }
    }

    /// Audio is RDP's, and refused on VNC by name.
    ///
    /// The error has to say which protocol carries it, because the mistake
    /// behind the key is a belief about what RFB does rather than a typo — and a
    /// target that silently ignored it would be a desktop that is simply quiet,
    /// with nothing anywhere to say why.
    #[test]
    fn audio_belongs_to_rdp_and_is_refused_on_vnc() {
        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "nope"
            protocol = "vnc"
            host = "10.0.0.5"
            audio = true
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("audio"), "{rendered}");
        assert!(rendered.contains("rdp"), "the protocol that does carry it is named: {rendered}");

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
    }

    /// An unset codec reads as Opus, and passthrough can be asked for by name.
    #[test]
    fn the_audio_codec_defaults_to_opus() {
        // Through `unwrap_or_default` because that is how every reader of the field
        // spells it — an unset codec becomes Opus at the call site, not at the parse.
        fn resolved(codec: Option<AudioCodec>) -> AudioCodec {
            codec.unwrap_or_default()
        }
        assert_eq!(resolved(None), AudioCodec::Opus);
        assert_eq!(resolved(Some(AudioCodec::Pcm)), AudioCodec::Pcm);

        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "10.0.0.5"
            audio = true
            audio_codec = "pcm"
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(config.targets[0].audio_codec, Some(AudioCodec::Pcm));
    }

    /// A codec without the audio it would encode is refused rather than ignored:
    /// the likely mistake behind it is a forgotten `audio = true`, and a silently
    /// accepted key would leave that looking like a codec that does not work.
    #[test]
    fn an_audio_codec_without_audio_is_refused() {
        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "10.0.0.5"
            audio_codec = "pcm"
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("audio_codec"), "{rendered}");
    }

    /// The codec names are the config's, not Rust's: `pcm`, never `Pcm`.
    #[test]
    fn an_unknown_audio_codec_is_refused_by_name() {
        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "10.0.0.5"
            audio = true
            audio_codec = "mp3"
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("audio_codec"), "{rendered}");
    }

    // ---- the adaptive dials --------------------------------------------------

    /// One valid target body per test below, parameterized by the keys under test.
    fn parse_target(body: &str) -> anyhow::Result<AppConfig> {
        ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "t"
            protocol = "rdp"
            host = "10.0.0.5"
            {body}
            "#,
            site_passwd_line()
        ))?
        .resolve()
    }

    /// The switch resolves into the plan with its default floor, on every
    /// strategy with a dial — and the plan says so.
    #[test]
    fn render_adaptive_resolves_a_floor_into_the_plan() {
        let cfg = parse_target(
            "render_type = \"video\"\nrender_quality = 80\nrender_adaptive = true",
        )
        .expect("adaptive video");
        let plan = cfg.targets[0].render_plan();
        assert_eq!(
            plan,
            RenderPlan::Video { quality: 80, adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN) }
        );
        assert_eq!(plan.describe(), "video q80 · adaptive ≥20");

        let cfg = parse_target(
            "render_type = \"fixed-quality\"\nrender_subtype = \"jpeg\"\n\
             render_quality = 70\nrender_adaptive = true\nrender_adaptive_min = 35",
        )
        .expect("adaptive tiles");
        let plan = cfg.targets[0].render_plan();
        assert_eq!(
            plan,
            RenderPlan::Tiles {
                base: TileCodec::Jpeg(70),
                motion: None,
                debug: false,
                adaptive: Some(35)
            }
        );
        assert_eq!(plan.describe(), "tiles · jpeg q70 · adaptive ≥35");

        let cfg = parse_target(
            "render_type = \"motion\"\nrender_motion_subtype = \"stream\"\n\
             render_motion_quality = 60\nrender_adaptive = true",
        )
        .expect("adaptive motion stream");
        assert_eq!(
            cfg.targets[0].render_plan().describe(),
            "motion · base lossless png, moving stream q60 · adaptive ≥20"
        );
    }

    /// A target that never asked stays exactly on its dial: no floor in the plan.
    #[test]
    fn without_render_adaptive_the_plan_has_no_floor() {
        let cfg = parse_target("render_type = \"video\"\nrender_quality = 80")
            .expect("plain video");
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Video { quality: 80, adaptive: None }
        );
    }

    /// `full` is lossless everywhere: there is no dial for the link to move.
    #[test]
    fn render_adaptive_on_full_is_refused() {
        let err = parse_target("render_adaptive = true").unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("render_adaptive"), "{rendered}");
        assert!(rendered.contains("full"), "{rendered}");
    }

    /// The floor belongs to the walk; without the walk nothing reads it.
    #[test]
    fn render_adaptive_min_without_the_walk_is_refused() {
        let err = parse_target(
            "render_type = \"video\"\nrender_quality = 80\nrender_adaptive_min = 30",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_adaptive_min"));
    }

    /// A floor above a configured quality leaves the walk nowhere to go —
    /// including above the *motion* quality, the smallest dial a motion plan has.
    #[test]
    fn a_floor_above_a_ceiling_is_refused() {
        let err = parse_target(
            "render_type = \"video\"\nrender_quality = 50\n\
             render_adaptive = true\nrender_adaptive_min = 60",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to go"));

        let err = parse_target(
            "render_type = \"motion\"\nrender_motion_subtype = \"jpeg\"\n\
             render_motion_quality = 10\nrender_adaptive = true\nrender_adaptive_min = 30",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to go"));

        // The *default* floor over the same low dial is no contradiction — the
        // operator never wrote it. It parses, and the walk clamps it to the
        // dial (`Congestion::new`) instead.
        parse_target(
            "render_type = \"motion\"\nrender_motion_subtype = \"jpeg\"\n\
             render_motion_quality = 10\nrender_adaptive = true",
        )
        .expect("a default floor clamps instead of refusing");
    }

    /// The audio keys resolve the same way the render dial does: defaults
    /// filled, kilobits become bits, and the floor is present exactly when the
    /// walk was asked for.
    #[test]
    fn the_audio_plan_resolves_defaults_and_the_adaptive_floor() {
        let cfg = parse_target("audio = true").expect("bare audio");
        assert_eq!(cfg.targets[0].audio_plan(), AudioPlan::default());
        assert_eq!(cfg.targets[0].audio_plan().bitrate_bps, 96_000);

        let cfg = parse_target("audio = true\naudio_bitrate = 128").expect("a rate");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan { codec: AudioCodec::Opus, bitrate_bps: 128_000, adaptive_floor_bps: None }
        );

        let cfg = parse_target("audio = true\naudio_adaptive = true").expect("adaptive");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan {
                codec: AudioCodec::Opus,
                bitrate_bps: 96_000,
                adaptive_floor_bps: Some(32_000)
            }
        );

        let cfg = parse_target(
            "audio = true\naudio_bitrate = 64\naudio_adaptive = true\naudio_bitrate_min = 24",
        )
        .expect("adaptive with both rates");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan {
                codec: AudioCodec::Opus,
                bitrate_bps: 64_000,
                adaptive_floor_bps: Some(24_000)
            }
        );
    }

    /// Passthrough has no encoder: every key that tunes one is refused beside it.
    #[test]
    fn the_bitrate_keys_are_opus_only() {
        let err = parse_target(
            "audio = true\naudio_codec = \"pcm\"\naudio_bitrate = 96",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("audio_bitrate"));

        let err = parse_target(
            "audio = true\naudio_codec = \"pcm\"\naudio_adaptive = true",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("audio_adaptive"));

        // And without audio at all, same rule one step up.
        let err = parse_target("audio_bitrate = 96").unwrap_err();
        assert!(format!("{err:#}").contains("audio_bitrate"));
    }

    /// The floor needs the walk, has a range, and must sit under the ceiling.
    #[test]
    fn the_audio_floor_is_validated_against_the_walk_and_the_ceiling() {
        let err = parse_target("audio = true\naudio_bitrate_min = 24").unwrap_err();
        assert!(format!("{err:#}").contains("audio_adaptive"));

        let err = parse_target(
            "audio = true\naudio_adaptive = true\naudio_bitrate_min = 4",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("6–510"));

        let err = parse_target(
            "audio = true\naudio_bitrate = 48\naudio_adaptive = true\naudio_bitrate_min = 48",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to go"));

        // The *default* floor above a low ceiling is no contradiction — the
        // operator never wrote it. It parses, and the walk clamps it to the
        // ceiling (`AudioCongestion::new`) instead.
        parse_target("audio = true\naudio_bitrate = 8\naudio_adaptive = true")
            .expect("a default floor clamps instead of refusing");

        let err = parse_target("audio = true\naudio_bitrate = 999").unwrap_err();
        assert!(format!("{err:#}").contains("6–510"));
    }
}
