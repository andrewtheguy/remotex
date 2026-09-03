//! Global TOML configuration: one `[server]` block and `[[targets]]` profiles.
//! Only the selected config file is read; target credentials remain server-side.
//!
//! One schema, read by two kinds of gateway — see [`Audience`]. The `[[targets]]`
//! half is identical for both, because a target is a target; `[server]` belongs to
//! the one a browser reaches.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use base64::Engine as _;
use bytes::Bytes;
use serde::Deserialize;

#[cfg(all(feature = "embedded-gateway", unix))]
use crate::auth::EmbeddedToken;
use crate::auth::{GatewayAuth, SitePasswd};
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

/// How a target's pixels travel — the *strategy*, the first of the two render
/// axes. The second, [`RenderSubtype`], is the codec of the base tiles, and a
/// lossy base also reads [`TargetConfig::render_quality`]. Two flat sibling
/// keys rather than a nested table, matching the rest of the target schema.
///
/// The two axes are orthogonal on purpose: this one says *what kind of thing
/// goes on the wire* (still tiles, tiles with a motion discount, one video
/// stream), the subtype says *what a base tile is encoded as* (lossless PNG, a
/// fixed-quality JPEG, or the classifier's per-tile choice between the two).
/// Every tiles-carrying strategy takes every subtype.
///
/// Only implemented strategies are variants; anything else is refused by serde
/// with the list of what is accepted. See docs/architecture.md for the dial.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderType {
    /// Every changed region as an independent still image at the base codec,
    /// and nothing else. The default; with the default subtype (lossless PNG)
    /// an unset target is byte-identical to the PNG-only gateway that preceded
    /// the dial.
    #[default]
    Tiles,
    /// The base encode, plus a second and much cheaper one for the cells changing
    /// fastest right now.
    ///
    /// Not a third way to encode every tile: it *builds on* the base a target
    /// would otherwise have, which is still what a settled cell is sent as. The
    /// base is read from [`RenderSubtype`] and [`TargetConfig::render_quality`],
    /// same as under [`RenderType::Tiles`].
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

/// How much colour a target's video streams carry per pixel, chosen per target
/// because it is a picture-against-decoder trade and only the operator knows which
/// browsers a target is watched from.
///
/// This is where the picture loss on a desktop stream actually is — not the
/// quantizer. Measured 2026-09-01 on 1280×800 of rendered text, coloured on a dark
/// terminal and black on white, encoded and decoded through libvpx: every 4:2:0
/// quantizer from the dial's finest to mathematically lossless lands at the same
/// 28.5 dB with a worst pixel 135 code values off, and so does the RGB→I420
/// conversion with no codec behind it at all. A one-pixel coloured glyph stem
/// shares its one colour sample with three background pixels and comes back at a
/// quarter of its saturation, and nothing downstream can put it back. The same
/// picture at 4:4:4 and the same quantizer measures 42.8 dB with a worst pixel 33
/// off. `a_444_stream_keeps_the_colour_420_averages_away` in [`crate::vp9`] is the
/// round trip that pins it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum Chroma {
    /// 4:2:0 — one colour sample per 2×2 pixels, VP9 profile 0. The default, and
    /// the one every VP9 decoder takes, hardware ones included.
    #[default]
    #[serde(rename = "420")]
    Subsampled,
    /// 4:4:4 — a colour sample per pixel, VP9 profile 1. On the picture above:
    /// a keyframe a third larger, inter frames no larger, a third more encode
    /// time, and coloured text that is the colour it was.
    ///
    /// The trade is the decoder. No hardware VP9 decoder takes profile 1, so
    /// this always decodes in software — Chromium does (measured headless,
    /// 2026-09-01), and a browser with no software VP9 at all, which is iOS and
    /// iPadOS, refuses the stream by name at `VideoDecoder.configure`, the same
    /// way it would refuse any configuration it lacks. Nothing falls back.
    #[serde(rename = "444")]
    Full,
}

impl Chroma {
    /// How the config key spells it, for messages that name it back.
    pub fn name(self) -> &'static str {
        match self {
            Self::Subsampled => "420",
            Self::Full => "444",
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
/// paired with [`RenderType`]. Under `tiles` that is every tile; under
/// [`RenderType::Motion`] it is every tile except the ones currently
/// in motion, which [`MotionSubtype`] names instead. [`RenderType::Video`] sends
/// no tiles and refuses the axis. All implemented codecs are
/// variants; serde refuses anything else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderSubtype {
    /// Lossless PNG. The default.
    #[default]
    Png,
    /// Baseline JPEG at [`TargetConfig::render_quality`]. Every tile goes to JPEG
    /// — there is no content classifier — so flat UI and text soften along with
    /// photographic content. That is the trade the fixed dial makes.
    Jpeg,
    /// Per tile, whichever of the two fits: a picture classifier
    /// ([`crate::classify`]) reads each tile's pixels and sends photographic
    /// content as JPEG at [`TargetConfig::render_quality`], everything else —
    /// flat UI, text — as lossless PNG. The classifier has no dial of its own;
    /// under [`RenderType::Motion`] it is the base, so a settled cell is
    /// classified and a moving one takes the motion encode as usual.
    Classify,
}

impl RenderSubtype {
    /// How the config key spells it, for messages that name it back.
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Classify => "classify",
        }
    }
}

/// The encode for what [`RenderType::Motion`] finds in motion — an axis of its own
/// rather than a reuse of [`RenderSubtype`].
///
/// This is the axis `stream` appears on, where the moving encode stops being a
/// still image at all — which it could only do by being nameable apart from the
/// base. It is also what lets a lossless base carry a lossy discount, since a `png`
/// base has no quality of its own to turn down. `png` is not a variant here and
/// never will be: a moving cell needs a quality to turn down, and lossless has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MotionSubtype {
    /// Baseline JPEG at [`TargetConfig::render_motion_quality`].
    Jpeg,
    /// A video stream per coalesced moving region, at
    /// [`TargetConfig::render_motion_quality`], with the base codec carrying
    /// everything else.
    ///
    /// The other one is a still picture per cell, re-encoded from scratch every
    /// frame; this is an inter-frame stream, which is what moving content is cheap
    /// in. What it costs instead is statefulness — an access unit means nothing out
    /// of sequence — and that is why it never reaches the client as a tile. See
    /// [`crate::regions`] for which regions get a stream and when one ends, and
    /// [`crate::protocol::VideoUnit`] for what arrives.
    ///
    /// Never the default: it has to be written out, because unlike `jpeg`
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
    /// Per tile, whichever of the other two [`crate::classify`] says fits:
    /// photographic content as JPEG at this quality (1–100), everything else
    /// as PNG. The decision runs on the encode worker, from the tile's own
    /// pixels, so it costs the read loops nothing.
    Classify {
        quality: u8,
        /// Outline the tiles the classifier sent as JPEG, in the pixels
        /// themselves, so QA reads the decision off the screen
        /// ([`TargetConfig::render_classify_debug`]). Carried here because the
        /// encoder is the one place the decision exists to be drawn.
        debug: bool,
    },
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
    /// quantizer is. The chroma is [`TargetConfig::render_chroma`], resolved.
    Stream { quality: u8, chroma: Chroma },
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
        /// [`TargetConfig::render_chroma`], resolved.
        chroma: Chroma,
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
                TileCodec::Classify { quality, debug } => {
                    let debug = if debug { " (debug outlines)" } else { "" };
                    format!("classified png / jpeg q{quality}{debug}")
                }
            }
        }
        // The floor as a suffix, because it modifies the whole plan rather than
        // one dial: every quality named before it is a ceiling the link may fall
        // below, and this is how far.
        fn floor(adaptive: Option<u8>) -> String {
            adaptive.map_or_else(String::new, |floor| format!(" · adaptive ≥{floor}"))
        }
        // Named only when it is not the default: 4:2:0 is what every stream was
        // before the key existed, and saying so on each card would be noise.
        fn chroma(chroma: Chroma) -> &'static str {
            match chroma {
                Chroma::Subsampled => "",
                Chroma::Full => " 4:4:4",
            }
        }
        match self {
            RenderPlan::Video { quality, adaptive, chroma: c } => {
                format!("video q{quality}{}{}", chroma(*c), floor(*adaptive))
            }
            RenderPlan::Tiles { base, motion: None, adaptive, .. } => {
                // No motion arm at all — plain `tiles`, whatever the base: whether
                // it is lossless is what the base already says.
                format!("tiles · {}{}", tile(*base), floor(*adaptive))
            }
            RenderPlan::Tiles { base, motion: Some(motion), debug, adaptive } => {
                let moving = match motion {
                    MotionEncode::Tile(codec) => tile(*codec),
                    MotionEncode::Stream { quality, chroma: c } => {
                        format!("stream q{quality}{}", chroma(*c))
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
    /// not the Screen Sharing password. On a plain `vnc` target it is the
    /// account a server checks through RealVNC's RSA-AES security types
    /// (wayvnc's `enable_auth` username, a RealVNC system account — see
    /// [`crate::vnc_rsa_aes`]); RFB `VncAuth` cannot carry a name, so a plain
    /// target that sets it must set [`Self::password`] with it.
    #[serde(default)]
    pub username: String,
    /// Password for [`Self::username`] (never leaves the server). On a plain
    /// `vnc` target it may stand alone, for an RSA-AES server that asks for a
    /// password and no name.
    #[serde(default)]
    pub password: String,
    /// A VNC server's own password — RFB `VncAuth`, which proves knowledge of a
    /// secret belonging to the *machine* and says nothing about who is
    /// connecting. Named apart from [`Self::password`] because on a Mac the two
    /// are different credentials that get you different screens: this is the
    /// Screen Sharing password, and it is answered with a login window of the
    /// connection's own (see [`crate::vnc`]).
    ///
    /// A plain `vnc` target's credential for a server offering `VncAuth`; it
    /// may sit beside [`Self::password`], and the server's offer decides which
    /// is answered. Rejected on other protocols and on either Apple
    /// [`Subtype`] — see [`ConfigFile::parse`].
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
    /// Security negotiation mode: `"auto"`, `"nla"`, or `"tls"`; `None` reads
    /// as [`Security::Auto`] ([`TargetConfig::security`]). RDP only — RFB
    /// negotiates its own security per the handshake — and `Option` rather than
    /// a bare default so that setting it on a VNC target is refused at parse
    /// time instead of accepted and left inert, the same rule as `egfx`.
    #[serde(default)]
    pub security: Option<Security>,
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
    /// Offer the remote a redirected camera (MS-RDPECAM). Rejected for VNC,
    /// like [`Self::audio`] and for the same shape of reason: RFB has no
    /// equivalent channel at all.
    ///
    /// Capability only. The device itself appears when a client enables the
    /// camera — explicitly, per session, never remembered — by opening
    /// `/ws/camera`; a target with this key and no such client offers the
    /// remote nothing. The browser encodes H.264 and the gateway passes it
    /// through, so there is no codec key beside this one.
    #[serde(default)]
    pub camera: bool,
    /// Offer the remote a redirected microphone (MS-RDPEAI). Rejected for VNC,
    /// like [`Self::audio`] and [`Self::camera`] and for the same shape of
    /// reason: RFB has no equivalent channel at all.
    ///
    /// Capability only, and the camera's twin: the microphone flows when a
    /// client enables it — explicitly, per session, never remembered — by
    /// opening `/ws/mic`; a target with this key and no such client offers the
    /// remote silence. The browser captures PCM and the gateway passes it
    /// through, so there is no codec key beside this one.
    #[serde(default)]
    pub microphone: bool,
    /// Render *strategy* for this target. Defaults to [`RenderType::Tiles`],
    /// which with the default subtype (lossless PNG) is byte-identical to
    /// before the dial existed. Validated against [`Self::render_subtype`] and
    /// [`Self::render_quality`] in [`ConfigFile::parse_with`]. Works for both
    /// RDP and VNC.
    #[serde(default)]
    pub render_type: RenderType,
    /// Codec for this target's base tiles; `None` reads as
    /// [`RenderSubtype::Png`] ([`TargetConfig::render_subtype`]). The legal
    /// pairing with [`Self::render_type`] is enforced at parse time, and it is an
    /// `Option` so that the pairing can see the key at all: `video` has no base
    /// tiles, and a `render_subtype` named beside it — `"png"` included, the
    /// value a bare default would have been indistinguishable from — is refused
    /// rather than accepted and left inert.
    #[serde(default)]
    pub render_subtype: Option<RenderSubtype>,
    /// The quality (1–100) of the base codec's lossy side: what
    /// [`RenderSubtype::Jpeg`] encodes every tile at, and what
    /// [`RenderSubtype::Classify`] encodes its photographic tiles at. Required
    /// exactly when the subtype is one of those two and refused for
    /// [`RenderSubtype::Png`], which is lossless and has no dial. `None`
    /// (unset) is the default.
    ///
    /// Under [`RenderType::Motion`] this is the *base* quality — what a settled
    /// cell gets — and it is omitted when the base is lossless PNG. Under
    /// [`RenderType::Video`], which has no tiles and no subtype, it is the one
    /// quality the stream holds.
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
    /// Chroma sampling of this target's video streams — `render_type = "video"`
    /// and `render_motion_subtype = "stream"` alike; `None` reads as
    /// [`Chroma::Subsampled`]. `Option` rather than a bare default so that
    /// setting it on a target that streams nothing is refused at parse time
    /// instead of accepted and left inert, the same rule as `audio_codec`
    /// without `audio`.
    #[serde(default)]
    pub render_chroma: Option<Chroma>,
    /// Outline every tile the classifier sends as JPEG, in the pixels
    /// themselves, so which regions it reads as photographic is visible on the
    /// screen instead of inferred from how soft something looks. A QA aid for
    /// [`RenderSubtype::Classify`] and refused for any other subtype; off
    /// unless asked for.
    ///
    /// The outline goes on the copy handed to the JPEG encoder, never on the
    /// pixels the shadow records as delivered — so the mark lasts exactly as
    /// long as the lossy tile it describes, and the next change repaints it
    /// away. PNG tiles are never marked: unmarked-and-sharp is the quiet
    /// majority, and outlining it would say nothing.
    #[serde(default)]
    pub render_classify_debug: bool,
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
    /// - A lossy tile codec (JPEG, base or motion) gets a quality per
    ///   *encode* instead of per session, scaled down linearly with that same
    ///   lag — Guacamole's curve, on this gateway's own signal.
    ///
    /// Refused for lossless PNG tiles — the one plan with no dial for this to
    /// move.
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
    ///
    /// A client that fits the desktop to its viewport and pinch-zooms
    /// ([`HostDisplay::fit`]) has a screen but not one to open at: it is the
    /// one client not showing the desktop at 100%, and its screen is a phone's
    /// or a tablet's. It takes the pinned size or the default, and its density
    /// still counts, elsewhere.
    pub fn opening_size(&self, display: Option<HostDisplay>) -> (u16, u16) {
        self.pinned_size()
            .or(display.filter(|d| !d.fit).map(|d| (d.w, d.h)))
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

    /// [`Self::security`] resolved: [`Security::Auto`] unless the operator chose.
    pub fn security(&self) -> Security {
        self.security.unwrap_or_default()
    }

    /// [`Self::render_subtype`] resolved: lossless PNG unless the operator chose.
    pub fn render_subtype(&self) -> RenderSubtype {
        self.render_subtype.unwrap_or_default()
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
        let chroma = self.render_chroma.unwrap_or_default();
        if let (RenderType::Video, Some(quality)) = (self.render_type, self.render_quality) {
            return RenderPlan::Video { quality, adaptive, chroma };
        }
        let base = match (self.render_subtype(), self.render_quality) {
            (RenderSubtype::Jpeg, Some(q)) => TileCodec::Jpeg(q),
            (RenderSubtype::Classify, Some(q)) => {
                TileCodec::Classify { quality: q, debug: self.render_classify_debug }
            }
            _ => TileCodec::Png,
        };
        let motion = match (self.render_type, self.render_motion_quality) {
            (RenderType::Motion, Some(q)) => match self.motion_subtype() {
                Some(MotionSubtype::Jpeg) => Some(MotionEncode::Tile(TileCodec::Jpeg(q))),
                Some(MotionSubtype::Stream) => Some(MotionEncode::Stream { quality: q, chroma }),
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
    /// The engines ask it too: a streaming target has every size it asks a remote for held
    /// under the stream's picture ceiling ([`crate::video::fit_ceiling`]), where a tiles
    /// target asks for the screen as it is.
    pub fn streams_video(&self) -> bool {
        match self.render_type {
            RenderType::Video => true,
            RenderType::Motion => self.motion_subtype() == Some(MotionSubtype::Stream),
            RenderType::Tiles => false,
        }
    }

    /// The motion encode this target asked for, defaulting off the base codec
    /// when the key is omitted — which `stream` never is, since a stream is not
    /// a cheaper version of a still. A `jpeg` base defaults to `jpeg`, and so
    /// does a `classify` base: its moving cells are changing too fast to be
    /// worth classifying, and the artifacts the classifier exists to keep off
    /// text are the ones motion hides anyway. `None` only for the pairing parse
    /// rejects: a `png` base with no motion subtype named.
    fn motion_subtype(&self) -> Option<MotionSubtype> {
        self.render_motion_subtype.or(match self.render_subtype() {
            RenderSubtype::Jpeg | RenderSubtype::Classify => Some(MotionSubtype::Jpeg),
            RenderSubtype::Png => None,
        })
    }
}

/// What a session opens at when neither the config nor the connecting client
/// named a size: no screen to measure, no operator to ask, one desk-shaped
/// answer.
///
/// Points, not backing pixels, and 16:9 rather than the 16:10 a working surface
/// would prefer, because of what it becomes at 2x: 3840×2160, exactly the 4K a
/// video stream encodes with the margin it has ([`crate::video::MAX_LONG_SIDE`]),
/// and the size every 4K panel and Mac virtual display is built around. It is
/// also what a phone gets: a touch client asks for this rather than its own
/// screen, which is portrait and far too small to be a desktop
/// (`sendMobileSize` in `frontend/src/useRemoteDesktop.ts`) — and a phone is
/// a 2x or 3x screen, so this is the one opening size that lands on the ceiling
/// rather than past it.
pub const DEFAULT_SIZE: (u16, u16) = (1920, 1080);

/// The port this project answers on when nothing says otherwise, in either
/// shape: [`DEFAULT_LISTEN`] below, and the TUI control plane's `--port`.
///
/// One number for both because they are two ways to serve, never two servers:
/// `remotex serve` is the deployed gateway and `remotex tui` is the local
/// control plane, and running them at once is the collision each refuses to
/// start into rather than a configuration to support.
pub const DEFAULT_PORT: u16 = 52380;

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
    // `ConfigFile::branding`), because an embedded config has no `[server]`
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
/// An embedded config has no `[server]` block at all ([`Audience::Embedded`]),
/// so a table that lived there could not name the instance — and accepting both
/// spellings would be two places to write one value, with the loser losing
/// silently. `deny_unknown_fields` refuses a file that still has anything of it
/// under `[server]`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrandingSection {
    /// Display name of this gateway: the browser's login screen, interstitials,
    /// tab title, and target picker.
    ///
    /// Defaults to [`DEFAULT_BRANDING`]; whitespace-only is treated as absent.
    pub text: Option<String>,
    /// The page's icon (`GET /api/logo`, the favicon of every client tab), as
    /// either a path to an image file or the image itself in a `data:` URL.
    ///
    /// One key for both because it is one thing — the icon — and which form it is
    /// written in is decided by the value: anything starting `data:` is the image,
    /// everything else is a path. A second key would let a config set both and
    /// leave the loser losing silently. See [`resolve_logo`]; unset means the page
    /// keeps no icon.
    pub logo: Option<String>,
}

/// The resolved branding: always a name, and an icon when one was configured.
#[derive(Clone, Debug)]
pub struct Branding {
    /// Display name for the login screen, interstitials, and browser tab title.
    pub text: String,
    /// The icon file, with its content type already decided.
    pub logo: Option<Logo>,
}

/// A configured logo, paired with the content type it is served under.
///
/// The pair exists so the one place that knows how to name an image
/// ([`logo_mime`], [`logo_media_type`]) runs at config resolution — a gateway
/// never serves an icon it could not name, and `check-config` refuses the value
/// before it is saved.
#[derive(Clone, Debug)]
pub struct Logo {
    pub source: LogoSource,
    pub mime: &'static str,
}

/// Where the icon's bytes come from.
#[derive(Clone, Debug)]
pub enum LogoSource {
    /// A file, read per request. As written in the config; a relative path
    /// resolves against the process's working directory, the same as
    /// `[server].static_dir`.
    File(PathBuf),
    /// The image itself, decoded once from the config's `data:` URL.
    ///
    /// [`Bytes`] rather than a `Vec`, because the whole [`AppConfig`] is cloned
    /// per request by the router's state and an icon that copied itself each time
    /// would be the one config value with a cost per hit.
    Inline(Bytes),
}

/// The content type `[branding].logo` is served under, from a file's extension.
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

/// The same closed list, reached from a `data:` URL's declared media type
/// instead of an extension. Lowercase in, canonical spelling out — a media type
/// is case-insensitive, and `image/vnd.microsoft.icon` is the registered name of
/// the type everything actually writes as `image/x-icon`.
fn logo_media_type(declared: &str) -> anyhow::Result<&'static str> {
    match declared {
        "image/png" => Ok("image/png"),
        "image/x-icon" | "image/vnd.microsoft.icon" => Ok("image/x-icon"),
        "image/svg+xml" => Ok("image/svg+xml"),
        "image/jpeg" => Ok("image/jpeg"),
        "image/gif" => Ok("image/gif"),
        "image/webp" => Ok("image/webp"),
        _ => anyhow::bail!(
            "[branding].logo declares {declared:?}, which is not an image a browser \
             tab can show — use image/png, image/x-icon, image/svg+xml, image/jpeg, \
             image/gif or image/webp"
        ),
    }
}

/// Read `[branding].logo`: a path to an image file, or the image itself.
///
/// The inline form is an ordinary `data:` URL —
/// `data:image/png;base64,iVBORw0…` — which is what makes one key enough. It is
/// self-describing, so the media type comes from the value rather than from an
/// extension the value does not have; it is what every tool that turns an image
/// into text already emits; and no path begins with it, so the two forms cannot
/// be confused for one another.
///
/// It exists for the configs that have nowhere to put a file: an instance
/// directory synced between machines, a container with one mounted config, a
/// `remotex.toml` pasted into a gist. The path form stays the better one whenever
/// there *is* somewhere — it survives an image being swapped without a restart,
/// and it keeps the config readable.
///
/// Whitespace inside the payload is dropped before decoding, so a blob wrapped at
/// 76 columns can be pasted straight into a TOML multi-line string the way
/// `base64` prints it.
fn resolve_logo(value: &str) -> anyhow::Result<Logo> {
    let value = value.trim();
    // A URI scheme is case-insensitive (RFC 3986 §3.1), and reading one spelling
    // only would send every other one to the path branch — where it fails, but
    // about an extension, which is not what is wrong with it.
    let scheme = value
        .get(.."data:".len())
        .filter(|prefix| prefix.eq_ignore_ascii_case("data:"));
    let Some(scheme) = scheme else {
        let path = PathBuf::from(value);
        return Ok(Logo { mime: logo_mime(&path)?, source: LogoSource::File(path) });
    };
    let uri = &value[scheme.len()..];

    let (declared, payload) = uri.split_once(',').context(
        "[branding].logo is a data: URL with no comma, so it has no image after \
         its media type",
    )?;
    let declared = declared.trim().to_ascii_lowercase();
    let declared = declared.strip_suffix(";base64").with_context(|| {
        format!(
            "[branding].logo is a data: URL that is not base64 ({declared:?}) — \
             write it as data:image/png;base64,<the encoded image>"
        )
    })?;
    let mime = logo_media_type(declared)?;

    // A wrapped blob is the normal shape of base64 in a file, and TOML keeps the
    // newlines of a multi-line string verbatim.
    let payload: String = payload.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&payload)
        .context("[branding].logo is a data: URL whose base64 does not decode")?;
    anyhow::ensure!(!bytes.is_empty(), "[branding].logo decodes to no image at all");
    Ok(Logo { mime, source: LogoSource::Inline(Bytes::from(bytes)) })
}

/// Who a config file is for, and therefore which rules it is held to.
///
/// The difference is not cosmetic — each audience makes a demand the other one
/// cannot meet — which is why this is a parameter of parsing rather than something
/// checked later by whoever happens to remember to:
///
/// - a [`Self::Served`] gateway is useless without a target to offer and a
///   credential to guard it, and it is told where to listen;
/// - an [`Self::Embedded`] one is started by a manager with the port, secret and
///   web root decided outside the config, so a `[server]` block could only
///   contradict them — and it may come up with **no targets at all**, because that
///   is a valid new instance and the picker's job is to say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// `remotex serve`: a browser's gateway.
    Served,
    /// `remotex serve-embedded`: a managed local instance.
    #[cfg(feature = "embedded-gateway")]
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
    /// A served gateway uses the configured TCP or Unix address. A managed local
    /// worker uses the private Unix socket supplied by its control plane.
    pub listen: ListenAddr,
    /// Directory holding the built frontend (index.html + assets), served from
    /// disk. Defaults to [`default_static_dir`] for a served gateway; an embedded
    /// one is given it by its launcher (`--web-root`).
    ///
    /// Every gateway has one, because every client is the same SPA and loads it
    /// from the gateway's own HTTP origin.
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
        #[cfg(feature = "embedded-gateway")]
        if audience == Audience::Embedded {
            // Refused rather than ignored, and named as a whole block rather than
            // key by key: every one of them is a decision the launcher has already
            // made for this gateway — a private Unix socket under the instance, a
            // web root it hands over on the command line
            // (`serve-embedded --web-root`), and a token instead of a login. A key
            // that is quietly overridden is worse than
            // one that is refused: it reads as configuration and behaves as
            // decoration.
            anyhow::ensure!(
                config.server.is_none(),
                "an embedded instance config may not have a [server] block: \
                 the launcher decides where its gateway listens, where the client it \
                 serves comes from, and how it authenticates. Only [branding] and \
                 [[targets]] belong here"
            );
        } else {
            anyhow::ensure!(
                !config.targets.is_empty(),
                "config has no [[targets]] — at least one target profile is required"
            );
        }
        #[cfg(not(feature = "embedded-gateway"))]
        {
            let _ = audience;
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
            // A pinned size is asked for as pixels at 1x, so the one oversize a
            // video stream refuses that check-config *can* see is a pin already
            // past the picture ceiling: at runtime the engines hold a screen under
            // it, but holding a pin would open at a size the operator did not
            // choose. (A pin under the ceiling at 1x may still land over it on a
            // 2x screen; that one is held, like a screen.)
            anyhow::ensure!(
                !target.streams_video()
                    || target.pinned_size().is_none_or(|(w, h)| {
                        crate::video::within_ceiling((u32::from(w), u32::from(h)))
                    }),
                "target {:?} pins a {:?}×{:?} size, but a video stream encodes at most a \
                 long side of {} and a short side of {} — pin a smaller size, leave the \
                 pin out, or give this target render_type = \"tiles\"",
                target.name,
                target.width,
                target.height,
                crate::video::MAX_LONG_SIDE,
                crate::video::MAX_SHORT_SIDE
            );
            // The chroma key describes a video stream, and a target with none has
            // nothing for it to describe — same rule as audio_codec without audio:
            // refused rather than accepted and left inert, because the likely
            // mistake behind it is a render_type that was never changed.
            anyhow::ensure!(
                target.render_chroma.is_none() || target.streams_video(),
                "target {:?} sets render_chroma, which only a video stream has — give this \
                 target render_type = \"video\", or render_type = \"motion\" with \
                 render_motion_subtype = \"stream\", or remove the key",
                target.name
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
            // And the security mode: TLS and NLA are RDP's negotiation, where
            // RFB settles its own in the handshake, so on a VNC target the key
            // names a choice nothing would read.
            anyhow::ensure!(
                target.security.is_none() || target.protocol == Protocol::Rdp,
                "target {:?} sets security on a {} target, and only rdp negotiates tls or \
                 nla — RFB settles its own security in the handshake. Remove the key.",
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
            // The camera is RDP's alone by the same rule: MS-RDPECAM is an RDP
            // channel and RFB has nothing to redirect a client's camera onto.
            anyhow::ensure!(
                !target.camera || target.protocol == Protocol::Rdp,
                "target {:?} sets camera on a {} target, and only rdp carries it: MS-RDPECAM \
                 is an RDP channel and RFB has no equivalent. Remove the key.",
                target.name,
                target.protocol.name()
            );
            // The microphone is RDP's alone by the same rule: MS-RDPEAI is an RDP
            // channel and RFB has nothing to redirect a client's microphone onto.
            anyhow::ensure!(
                !target.microphone || target.protocol == Protocol::Rdp,
                "target {:?} sets microphone on a {} target, and only rdp carries it: MS-RDPEAI \
                 is an RDP channel and RFB has no equivalent. Remove the key.",
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
            // Which credentials a VNC target may carry is the subtype's to say:
            // an Apple subtype authenticates an account to a Mac and nothing else,
            // while a plain target carries an account for RSA-AES, the machine's
            // secret for VncAuth, or both for the server's offer to decide. A
            // credential is refused where it cannot be used rather than quietly
            // ignored, which is how a password ends up authenticating nobody.
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
                        target.username.is_empty() || !target.password.is_empty(),
                        "target {:?} is protocol \"vnc\" and sets username without password — \
                         RSA-AES carries the two together; a VNC server's own password goes \
                         in vnc_password, and a Mac account under subtype = \"ard\"",
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
            // The render dial has two axes and they are validated together,
            // because only some pairings mean anything and `render_quality`
            // belongs to exactly one of them. The match is exhaustive so a future
            // variant cannot be added without deciding what it pairs with here.
            match (target.render_type, target.render_subtype) {
                (RenderType::Tiles, None | Some(RenderSubtype::Png)) => {
                    anyhow::ensure!(
                        target.render_quality.is_none(),
                        "target {:?} sets render_quality, which the lossless \"png\" \
                         subtype has no use for. Set render_subtype = \"jpeg\" for a fixed \
                         lossy quality, or \"classify\" to spend it only on photographic \
                         tiles",
                        target.name
                    );
                }
                // The two lossy bases make the same demand for the same reason:
                // `jpeg` spends the quality on every tile, `classify` only on
                // the ones its classifier reads as photographic, and neither
                // has a default — a quality nobody chose is not a quality.
                (RenderType::Tiles, Some(RenderSubtype::Jpeg | RenderSubtype::Classify)) => {
                    let q = target.render_quality.with_context(|| format!(
                        "target {:?} sets a lossy render_subtype but no render_quality — \
                         it needs one, an integer 1–100",
                        target.name
                    ))?;
                    anyhow::ensure!(
                        (1..=100).contains(&q),
                        "target {:?} sets render_quality = {q}, which is out of range — it \
                         must be 1–100",
                        target.name
                    );
                }
                // `motion` reads the base off the subtype and the quality rather
                // than off `render_type`, which it occupies itself: a `png` base
                // is lossless and takes no quality, a lossy base needs one. This
                // is the only strategy that can express a lossless base with a
                // lossy discount, which is the interesting one — text and flat UI
                // stay perfect and only what moves gets ugly.
                (RenderType::Motion, None | Some(RenderSubtype::Png)) => {
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
                         a PNG base must name its own: \"jpeg\", or \"stream\" for a \
                         video stream per moving region",
                        target.name
                    );
                }
                // Same rule for both lossy bases, and under `motion` the
                // classifier is at its most natural: a settled cell is
                // classified — photographic goes JPEG, text stays lossless —
                // while a moving cell takes the motion encode either way.
                (RenderType::Motion, Some(RenderSubtype::Jpeg | RenderSubtype::Classify)) => {
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
                (RenderType::Video, None) => {
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
                // Every value is refused here, `png` included: it is the default the
                // key would otherwise read as, but a key that was written names an
                // expectation, and on this strategy nothing would ever read it.
                (RenderType::Video, Some(subtype)) => {
                    anyhow::bail!(
                        "target {:?} sets render_type \"video\" with render_subtype = {:?}. \
                         render_subtype names a codec for each changed region separately, and \
                         \"video\" does not send regions at all — it sends the whole desktop as \
                         one video stream, where every frame depends on the one before it. Drop \
                         render_subtype to keep \"video\", or set render_type = \"tiles\" to \
                         keep this subtype",
                        target.name,
                        subtype.name()
                    )
                }
            }
            // The debug outlines belong to the classifier: no other subtype
            // has a per-tile decision to draw.
            anyhow::ensure!(
                target.render_subtype() == RenderSubtype::Classify || !target.render_classify_debug,
                "target {:?} sets render_classify_debug without render_subtype = \
                 \"classify\" — the outlines show which tiles the classifier sent as JPEG, \
                 and no other subtype makes that decision",
                target.name
            );
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
            // The adaptive switch needs a dial to move. The one plan without one
            // is lossless tiles: a `motion` plan always has at least the motion
            // quality, `video` has its own, and a lossy base carries one.
            anyhow::ensure!(
                !target.render_adaptive
                    || target.render_type != RenderType::Tiles
                    || target.render_subtype() != RenderSubtype::Png,
                "target {:?} sets render_adaptive on lossless PNG tiles, which have no \
                 quality for the link to move. Pick a plan with a lossy dial — a \"jpeg\" \
                 or \"classify\" subtype, \"motion\", or \"video\"",
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

    /// Resolve the runtime configuration of a managed local instance: its private
    /// Unix socket, the SPA from the launcher's web root, and a freshly minted
    /// token.
    ///
    /// Every one of those is an argument here rather than a default that
    /// `[server]` could override, which is what [`Audience::Embedded`] enforces on
    /// the way in. `[branding]` is the one thing such a config *may* say about the
    /// gateway itself: it names the instance, and multiple local instances are
    /// easier to tell apart if they can be called different things.
    #[cfg(all(feature = "embedded-gateway", unix))]
    pub fn resolve_embedded(
        self,
        token: EmbeddedToken,
        web_root: PathBuf,
        socket_path: PathBuf,
    ) -> anyhow::Result<AppConfig> {
        Ok(AppConfig {
            // Only the native control plane reaches this listener. It owns the TCP
            // origin a browser addresses and proxies both HTTP and WebSockets here.
            listen: ListenAddr::Unix(socket_path),
            static_dir: web_root,
            targets: self.targets,
            auth: GatewayAuth::Token(token),
            branding: Self::resolve_branding(self.branding.as_ref())?,
            dev_hostname: None,
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
                .as_deref()
                .map(resolve_logo)
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
        // Refused here, at the one place the address is read, rather than at the
        // bind: `std::os::unix::net` does not exist on Windows, so a gateway there
        // has no socket to offer and should say so before it reports a listener.
        #[cfg(not(unix))]
        anyhow::bail!(
            "{UNIX_LISTEN_PREFIX}{path} — Unix sockets are not supported on Windows; \
             listen on host:port, as in \"{DEFAULT_LISTEN}\""
        );
        #[cfg(unix)]
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

/// Validate candidate config text the way a deployed browser gateway reads it.
///
/// This lives in the ordinary config module so `check-config` remains useful in
/// feature-minimal builds without pulling in the managed-instance substrate.
pub fn check(text: &str) -> anyhow::Result<()> {
    ConfigFile::parse(text)?.resolve().map(|_| ())
}

/// Read config text from a file, or from stdin when no path is given.
///
/// Stdin accepts text from an editor that has not been saved, so there is no file
/// to name yet.
pub fn read_candidate(path: Option<&Path>) -> anyhow::Result<String> {
    match path {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display())),
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("failed to read the config from stdin")?;
            Ok(text)
        }
    }
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

    // The Windows package installs the same tree under %ProgramFiles%\remotex, and
    // the tree is relocatable: <root>\bin\remotex.exe beside <root>\share\remotex\web.
    // Its configuration lives outside that tree, under %ProgramData%, for the
    // same reason as /etc above — replacing the unpacked release must not touch a
    // file holding credentials.
    #[cfg(windows)]
    if bin_dir.file_name().is_some_and(|name| name.eq_ignore_ascii_case("bin"))
        && let Some(root) = bin_dir.parent()
        && let Some(program_data) = std::env::var_os("ProgramData")
    {
        return Some(InstalledLayout {
            config: PathBuf::from(program_data).join("remotex").join("remotex.toml"),
            static_dir: root.join("share").join("remotex").join("web"),
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
/// serve is a browser window showing a 404, which does not say which of the two
/// ends is wrong.
///
/// `hint` is the half that differs: a served gateway is told where to look in its
/// config, and an embedded one is told the launcher supplied this path — the config
/// it reads has no key for it and `[server]` is refused there.
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

        // The quick installer's tree is a Unix one; on Windows any `bin` directory is
        // the package's tree, which is the arm below.
        #[cfg(unix)]
        {
            let quick = installed_layout_for_exe(Path::new(
                "/srv/remotex/versions/0.0.144/bin/remotex",
            ))
            .unwrap();
            assert_eq!(quick.config, Path::new("/srv/remotex/etc/remotex.toml"));
            assert_eq!(
                quick.static_dir,
                Path::new("/srv/remotex/versions/0.0.144/share/remotex/web")
            );
        }

        #[cfg(windows)]
        {
            let installed = installed_layout_for_exe(Path::new(
                r"C:\Program Files\remotex\bin\remotex.exe",
            ))
            .unwrap();
            let program_data = PathBuf::from(std::env::var_os("ProgramData").unwrap());
            assert_eq!(installed.config, program_data.join("remotex").join("remotex.toml"));
            assert_eq!(
                installed.static_dir,
                Path::new(r"C:\Program Files\remotex\share\remotex\web")
            );
        }

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

    /// Two constants, one number: the served default address and the port the TUI
    /// takes when nothing says otherwise cannot drift apart silently.
    #[test]
    fn the_default_listen_address_is_the_default_port() {
        assert_eq!(DEFAULT_LISTEN, format!("127.0.0.1:{DEFAULT_PORT}"));
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let config = ConfigFile::parse(&minimal()).unwrap().resolve().unwrap();
        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
        let site_passwd = config.auth.login().expect("a served gateway logs in");
        assert_eq!(site_passwd.username(), "admin");
        assert_eq!(config.targets.len(), 1);
        let t = &config.targets[0];
        assert_eq!(t.name, "one");
        assert_eq!(t.protocol, Protocol::Rdp);
        assert_eq!((t.host.as_str(), t.port), ("192.0.2.10", 3389));
        assert_eq!(t.pinned_size(), None, "an unpinned size follows the client's screen");
        assert_eq!(t.default_size(), DEFAULT_SIZE);
        assert_eq!(t.security(), Security::Auto);
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
    #[cfg(unix)]
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

    /// Where there are no Unix sockets the address is refused by name, before any
    /// bind — not reported as a listener that then fails to exist.
    #[cfg(not(unix))]
    #[test]
    fn a_unix_socket_is_refused_where_there_are_none() {
        let err = ConfigFile::parse(&with_server(r#"listen = "unix:/run/gw.sock""#))
            .and_then(ConfigFile::resolve)
            .expect_err("no Unix sockets on this platform");
        let text = format!("{err:#}");
        assert!(text.contains("not supported on Windows"), "{text}");
        assert!(text.contains("unix:/run/gw.sock"), "it names the address: {text}");
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
        let LogoSource::File(path) = &logo.source else {
            panic!("a plain string is a path");
        };
        assert_eq!(path, &PathBuf::from("/etc/remotex/acme.png"));
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

    /// A 1×1 PNG, so the inline tests carry a real image rather than an arbitrary
    /// blob that happens to be base64.
    const PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    /// The image written into the config instead of beside it. Decoded once, here,
    /// so a `data:` URL that is not one fails `check-config` and not the tab.
    #[test]
    fn a_data_url_logo_is_decoded_at_resolution() {
        let logo = resolve_logo(PNG_DATA_URL).expect("a data: URL is the image itself");
        assert_eq!(logo.mime, "image/png");
        let LogoSource::Inline(bytes) = &logo.source else {
            panic!("a data: URL is not a path");
        };
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "the PNG signature");

        // Through a real config, and wrapped the way `base64` prints it: TOML keeps
        // a multi-line string's newlines, so the payload arrives with them in it.
        let wrapped = PNG_DATA_URL.replace("base64,", "base64,\n").replace("AAAA", "AAAA\n  ");
        let toml = format!("[branding]\nlogo = \"\"\"\n{wrapped}\n\"\"\"\n{}", minimal());
        let config = ConfigFile::parse(&toml).unwrap().resolve().unwrap();
        let Some(Logo { source: LogoSource::Inline(from_file), mime }) = config.branding.logo
        else {
            panic!("the wrapped data: URL is the same image");
        };
        assert_eq!(mime, "image/png");
        assert_eq!(&from_file, bytes, "the wrapping is not part of the image");

        // The media type is the value's, not an extension's, and it is canonical:
        // a case a browser would take either way arrives spelled one way.
        let ico = resolve_logo("DATA:IMAGE/VND.MICROSOFT.ICON;BASE64,AAAA").unwrap();
        assert_eq!(ico.mime, "image/x-icon");
        // Including the scheme, which is case-insensitive and is the one part that
        // decides which branch the value takes at all.
        let mixed = resolve_logo("dAtA:image/GIF;Base64,AAAA").unwrap();
        assert_eq!(mixed.mime, "image/gif");
        assert!(matches!(mixed.source, LogoSource::Inline(_)), "not a path");
    }

    /// Every way a `data:` logo can be wrong says which way it was wrong, because
    /// the operator is looking at one long line of base64 either way.
    #[test]
    fn a_data_url_that_is_not_an_image_is_refused() {
        for (value, expected) in [
            // Not base64 — a data: URL may carry percent-encoded text, and that is
            // not a thing this reads.
            ("data:image/png,%89PNG", "not base64"),
            // Base64 of something no tab can show.
            ("data:application/pdf;base64,JVBERi0=", "not an image"),
            ("data:;base64,AAAA", "not an image"),
            // Base64 that is not base64.
            ("data:image/png;base64,not valid!", "does not decode"),
            // Well-formed and empty, which is a tab with a broken icon rather than
            // the no-icon a missing key gets.
            ("data:image/png;base64,", "no image at all"),
            ("data:image/png;base64", "no comma"),
        ] {
            let error = format!("{:#}", resolve_logo(value).unwrap_err());
            assert!(error.contains(expected), "{value:?} said {error}");
            assert!(error.contains("[branding].logo"), "{error}");
        }
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
        assert_eq!(win.security(), Security::Nla);
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
        assert_eq!(t.render_type, RenderType::Tiles);
        assert_eq!(t.render_subtype, None, "an unset base reads as png without being one");
        assert_eq!(t.render_subtype(), RenderSubtype::Png);
        assert_eq!(t.render_quality, None);
        assert_eq!(
            t.render_plan(),
            RenderPlan::Tiles { base: TileCodec::Png, motion: None, debug: false, adaptive: None }
        );
    }

    #[test]
    fn tiles_with_a_jpeg_base_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "tiles"
            render_subtype = "jpeg"
            render_quality = 60
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_type, RenderType::Tiles);
        assert_eq!(t.render_subtype(), RenderSubtype::Jpeg);
        assert_eq!(t.render_quality, Some(60));
        assert_eq!(
            t.render_plan(),
            RenderPlan::Tiles { base: TileCodec::Jpeg(60), motion: None, debug: false, adaptive: None }
        );
    }

    /// The subtype is the codec axis, so a lossy one needs no particular
    /// render_type: `tiles` is the default, and naming it changes nothing.
    #[test]
    fn a_lossy_subtype_needs_no_explicit_render_type() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_subtype = "jpeg"
            render_quality = 60
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles { base: TileCodec::Jpeg(60), motion: None, debug: false, adaptive: None }
        );
    }

    #[test]
    fn tiles_with_a_classify_base_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "tiles"
            render_subtype = "classify"
            render_quality = 60
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_subtype(), RenderSubtype::Classify);
        assert_eq!(
            t.render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Classify { quality: 60, debug: false },
                motion: None,
                debug: false,
                adaptive: None
            }
        );
    }

    #[test]
    fn classify_without_a_quality_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_subtype = "classify"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_quality"), "{err:#}");
    }

    /// The classifier as a `motion` base — the pairing the subtype axis exists
    /// to make expressible: a settled cell is classified (photographic JPEG,
    /// text lossless), a moving cell takes the motion encode, which defaults
    /// to `jpeg` because a cell changing fast is not worth classifying.
    #[test]
    fn motion_takes_a_classify_base() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_subtype = "classify"
            render_quality = 60
            render_motion_quality = 15
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Classify { quality: 60, debug: false },
                motion: Some(MotionEncode::Tile(TileCodec::Jpeg(15))),
                debug: false,
                adaptive: None
            }
        );
    }

    /// And with `stream` written out, the moving regions become video while the
    /// settled ones are still classified.
    #[test]
    fn motion_streams_over_a_classify_base() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_subtype = "classify"
            render_quality = 60
            render_motion_subtype = "stream"
            render_motion_quality = 30
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Classify { quality: 60, debug: false },
                motion: Some(MotionEncode::Stream { quality: 30, chroma: Chroma::Subsampled }),
                debug: false,
                adaptive: None
            }
        );
    }

    /// The chroma key reaches both kinds of stream and defaults to what every
    /// stream was before it existed.
    #[test]
    fn render_chroma_reaches_the_stream_and_defaults_to_420() {
        let video = |extra: &str| {
            ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                render_type = "video"
                render_quality = 100
                {extra}
                "#
            ))
            .unwrap()
            .targets[0]
                .render_plan()
        };
        assert_eq!(
            video(""),
            RenderPlan::Video { quality: 100, adaptive: None, chroma: Chroma::Subsampled }
        );
        assert_eq!(
            video("render_chroma = \"420\""),
            RenderPlan::Video { quality: 100, adaptive: None, chroma: Chroma::Subsampled }
        );
        assert_eq!(
            video("render_chroma = \"444\""),
            RenderPlan::Video { quality: 100, adaptive: None, chroma: Chroma::Full }
        );
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_motion_subtype = "stream"
            render_motion_quality = 30
            render_chroma = "444"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Png,
                motion: Some(MotionEncode::Stream { quality: 30, chroma: Chroma::Full }),
                debug: false,
                adaptive: None
            }
        );
    }

    /// A chroma for a target that streams nothing is refused, like a codec for
    /// audio that was never turned on; and the key takes only the two samplings
    /// VP9 profiles 0 and 1 are.
    #[test]
    fn render_chroma_without_a_stream_is_refused() {
        for keys in [
            "",
            "render_subtype = \"jpeg\"\nrender_quality = 60",
            "render_type = \"motion\"\nrender_motion_subtype = \"jpeg\"\nrender_motion_quality = 30",
        ] {
            let err = ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                {keys}
                render_chroma = "444"
                "#
            ))
            .unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("render_chroma"), "{keys:?}: {message}");
            assert!(message.contains("video stream"), "{keys:?}: {message}");
        }
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "video"
            render_quality = 100
            render_chroma = "422"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("422"), "{err:#}");
    }

    /// The outlines are the classifier's own debug aid, and resolve into the
    /// plan's codec so the encoder — the place the decision is made — sees it.
    #[test]
    fn the_classify_debug_overlay_is_opt_in_and_belongs_to_classify() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_subtype = "classify"
            render_quality = 60
            render_classify_debug = true
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Classify { quality: 60, debug: true },
                motion: None,
                debug: false,
                adaptive: None
            }
        );

        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_subtype = "jpeg"
            render_quality = 60
            render_classify_debug = true
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_classify_debug"), "{err:#}");
    }

    #[test]
    fn a_lossy_subtype_without_a_quality_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
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
        assert_eq!(cfg.targets[0].render_plan(), RenderPlan::Video { quality: 60, adaptive: None, chroma: Chroma::Subsampled });
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
    ///
    /// Every value, `png` included: that one is what an omitted key reads as, so
    /// it is the value a bare default would have let through — and a key that was
    /// written names an expectation `video` cannot meet, whatever it says.
    #[test]
    fn video_refuses_a_render_subtype() {
        for subtype in ["png", "jpeg", "classify"] {
            let err = ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                render_type = "video"
                render_subtype = "{subtype}"
                render_quality = 60
                "#
            ))
            .unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("render_subtype"), "the message should name the axis: {msg}");
            assert!(msg.contains(subtype), "the message should name the value: {msg}");
            assert!(msg.contains("video stream"), "the message should say what video is: {msg}");
            assert!(msg.contains("tiles"), "the message should say the way out: {msg}");
        }
    }

    /// The same key is the default it names everywhere else: `render_subtype =
    /// "png"` under `tiles` or `motion` is exactly what leaving it out is, and
    /// the refusal above is about `video` having no base tiles, not about the
    /// value.
    #[test]
    fn an_explicit_png_base_is_the_default_under_tiles_and_motion() {
        for keys in [
            "render_type = \"tiles\"",
            "render_type = \"motion\"\nrender_motion_subtype = \"jpeg\"\nrender_motion_quality = 30",
        ] {
            let explicit = format!(
                "[[targets]]\nname = \"a\"\nprotocol = \"rdp\"\nhost = \"h\"\n{keys}\n\
                 render_subtype = \"png\"\n"
            );
            let implicit = format!(
                "[[targets]]\nname = \"a\"\nprotocol = \"rdp\"\nhost = \"h\"\n{keys}\n"
            );
            let explicit = ConfigFile::parse(&explicit).unwrap_or_else(|e| panic!("{keys}: {e:#}"));
            let implicit = ConfigFile::parse(&implicit).unwrap_or_else(|e| panic!("{keys}: {e:#}"));
            assert_eq!(explicit.targets[0].render_subtype, Some(RenderSubtype::Png), "{keys}");
            assert_eq!(implicit.targets[0].render_subtype, None, "{keys}");
            assert_eq!(
                explicit.targets[0].render_plan(),
                implicit.targets[0].render_plan(),
                "{keys}: naming the default changes nothing"
            );
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
    fn render_quality_on_lossless_png_is_rejected() {
        // render_type/subtype default to tiles/png, so a stray quality has
        // nothing to apply to — with or without the defaults written out.
        for keys in ["", "render_type = \"tiles\"\nrender_subtype = \"png\"\n"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                host = "h"
                {keys}render_quality = 50
                "#
            );
            let err = ConfigFile::parse(&toml).unwrap_err();
            assert!(format!("{err:#}").contains("lossless"), "{err:#}");
        }
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
            msg.contains("tiles") && msg.contains("motion") && msg.contains("video"),
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
        assert_eq!(t.render_subtype(), RenderSubtype::Png);
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
            render_subtype = "jpeg"
            render_quality = 60
            render_motion_quality = 10
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Jpeg(60),
                motion: Some(MotionEncode::Tile(TileCodec::Jpeg(10))),
                debug: false,
                adaptive: None
            }
        );
    }

    /// The reason the motion encode is an axis of its own: what a settled cell gets
    /// is a still picture, and what is moving may stop being one at all, so it need
    /// not be what the base resolved to.
    #[test]
    fn the_motion_encode_need_not_be_the_base_codec() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            host = "h"
            render_type = "motion"
            render_subtype = "jpeg"
            render_quality = 60
            render_motion_subtype = "stream"
            render_motion_quality = 10
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Jpeg(60),
                motion: Some(MotionEncode::Stream { quality: 10, chroma: Chroma::Subsampled }),
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
            ("tiles over lossless png, the default", "render_type = \"tiles\"", "tiles · lossless png"),
            (
                "tiles over fixed-quality jpeg",
                "render_subtype = \"jpeg\"\nrender_quality = 60",
                "tiles · jpeg q60",
            ),
            (
                "tiles behind the classifier",
                "render_subtype = \"classify\"\nrender_quality = 60",
                "tiles · classified png / jpeg q60",
            ),
            (
                "the classifier's debug outlines, a different session to be looking at",
                "render_subtype = \"classify\"\nrender_quality = 60\nrender_classify_debug = true",
                "tiles · classified png / jpeg q60 (debug outlines)",
            ),
            (
                "motion over a classify base",
                "render_type = \"motion\"\nrender_subtype = \"classify\"\nrender_quality = 60\nrender_motion_quality = 15",
                "motion · base classified png / jpeg q60, moving jpeg q15",
            ),
            (
                "motion over a lossless base",
                "render_type = \"motion\"\nrender_motion_subtype = \"jpeg\"\nrender_motion_quality = 30",
                "motion · base lossless png, moving jpeg q30",
            ),
            (
                "motion whose moving encode defaults to the base",
                "render_type = \"motion\"\nrender_subtype = \"jpeg\"\nrender_quality = 70\nrender_motion_quality = 35",
                "motion · base jpeg q70, moving jpeg q35",
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
            (
                "the whole desktop as one stream with every pixel's colour",
                "render_type = \"video\"\nrender_quality = 60\nrender_chroma = \"444\"",
                "video q60 4:4:4",
            ),
            (
                "a stream per region with every pixel's colour",
                "render_type = \"motion\"\nrender_motion_subtype = \"stream\"\nrender_motion_quality = 40\nrender_chroma = \"444\"",
                "motion · base lossless png, moving stream q40 4:4:4",
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
                motion: Some(MotionEncode::Stream { quality: 30, chroma: Chroma::Subsampled }),
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
            render_subtype = "jpeg"
            render_quality = 60
            render_motion_quality = 30
            "#,
        )
        .unwrap();
        assert_eq!(
            motion_of(cfg.targets[0].render_plan()),
            Some(MotionEncode::Tile(TileCodec::Jpeg(30))),
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
                render_type = "tiles"
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
        assert!(msg.contains("jpeg"), "{msg}");
    }

    /// Motion is a shared sink strategy, independent of which engine produced the
    /// damage. Apple High Performance therefore gets the same plan as every other
    /// VNC subtype, including when its virtual display can resize.
    #[test]
    fn motion_is_accepted_on_apple_high_performance() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "vnc"
            subtype = "ard-high-performance"
            host = "h"
            username = "u"
            password = "p"
            resize = true
            render_type = "motion"
            render_motion_subtype = "jpeg"
            render_motion_quality = 10
            "#,
        )
        .expect("motion is independent of the VNC subtype");
        assert_eq!(
            motion_of(cfg.targets[0].render_plan()),
            Some(MotionEncode::Tile(TileCodec::Jpeg(10)))
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
            render_type = "tiles"
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

    /// An `rdp` target body, with whatever keys the case is about.
    fn rdp_toml(extra: &str) -> String {
        format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "192.0.2.10"
            {extra}
            "#,
            site_passwd_line()
        )
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

    /// A plain `vnc` target carries an account for RSA-AES, the machine's
    /// secret for VncAuth, or both; what it cannot carry is half an account,
    /// because no security type takes a name without a password.
    #[test]
    fn a_plain_vnc_target_takes_an_account_or_the_servers_own_password() {
        let err = ConfigFile::parse(&vnc_toml(r#"username = "andrew""#)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("username without password") && msg.contains("RSA-AES"), "{msg}");

        let account = ConfigFile::parse(&vnc_toml("username = \"andrew\"\npassword = \"hunter2\""))
            .unwrap();
        assert_eq!(account.targets[0].username, "andrew");
        assert_eq!(account.targets[0].password, "hunter2");
        assert!(account.targets[0].subtype.is_none());
        // A password alone is an RSA-AES server that asks for no name.
        assert!(ConfigFile::parse(&vnc_toml(r#"password = "hunter2""#)).is_ok());
        // Both credentials at once leave the choice to the server's offer.
        let both = ConfigFile::parse(&vnc_toml(
            "username = \"andrew\"\npassword = \"hunter2\"\nvnc_password = \"secret\"",
        ))
        .unwrap();
        assert_eq!(both.targets[0].vnc_password, "secret");

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
        let screen = HostDisplay { w: 1728, h: 1117, scale: 200, fit: false };

        let pinned = &ConfigFile::parse(&vnc_toml("width = 1600\nheight = 1000")).unwrap().targets[0];
        assert_eq!(pinned.opening_size(Some(screen)), (1600, 1000));
        assert_eq!(pinned.default_size(), (1600, 1000));

        let free = &ConfigFile::parse(&vnc_toml("")).unwrap().targets[0];
        assert_eq!(free.opening_size(Some(screen)), (1728, 1117));
        assert_eq!(free.opening_size(None), DEFAULT_SIZE);
        assert_eq!(free.default_size(), DEFAULT_SIZE);

        // A pinch-zoom client's screen is not an opening size: the pinned size
        // still wins, and without one it opens at the default rather than at a
        // phone's shape.
        let phone = HostDisplay { w: 430, h: 932, scale: 300, fit: true };
        assert_eq!(pinned.opening_size(Some(phone)), (1600, 1000));
        assert_eq!(free.opening_size(Some(phone)), DEFAULT_SIZE);

        let err = ConfigFile::parse(&vnc_toml("width = 1600")).unwrap_err();
        assert!(
            format!("{err:#}").contains("sets width without height"),
            "{err:#}"
        );
    }

    /// A zero axis is refused on every target alike — a High Performance
    /// virtual display was merely the first place it was caught misbehaving.
    /// The one oversize check-config can see: a pin a video stream would refuse
    /// at 1x. The same pin on a tiles target is an oversized desktop that scrolls.
    #[test]
    fn a_pinned_size_over_the_video_ceiling_is_refused_only_where_it_streams() {
        let pin = "width = 5120\nheight = 2880\n";
        let err = ConfigFile::parse(&rdp_toml(&format!(
            "{pin}render_type = \"video\"\nrender_quality = 60"
        )))
        .expect_err("a 5K pin on a video stream parsed");
        assert!(format!("{err:#}").contains("3840"), "{err:#}");
        assert!(format!("{err:#}").contains("tiles"), "{err:#}");
        ConfigFile::parse(&rdp_toml(&format!(
            "{pin}render_type = \"motion\"\nrender_subtype = \"jpeg\"\nrender_quality = 60\n\
             render_motion_subtype = \"stream\"\nrender_motion_quality = 60"
        )))
        .expect_err("a 5K pin on a region stream parsed");
        ConfigFile::parse(&rdp_toml(&format!("{pin}render_type = \"tiles\"")))
            .expect("a 5K pin on a tiles target is an oversized desktop that scrolls");
        for pin in ["width = 3840\nheight = 2400", "width = 2400\nheight = 3840"] {
            ConfigFile::parse(&rdp_toml(&format!("{pin}\nrender_type = \"video\"\nrender_quality = 60")))
                .expect("a 4K pin, either way up, is a picture the stream takes");
        }
    }

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

    /// The security mode is RDP's negotiation, and refused on VNC by name —
    /// every value, `auto` included, since the mistake is a key that nothing on
    /// this protocol would read, not the value chosen for it.
    #[test]
    fn security_belongs_to_rdp_and_is_refused_on_vnc() {
        for value in ["auto", "nla", "tls"] {
            for subtype in ["", "subtype = \"ard\"\nusername = \"u\"\npassword = \"p\""] {
                let err = ConfigFile::parse(&format!(
                    r#"
                    [server]
                    {}

                    [[targets]]
                    name = "nope"
                    protocol = "vnc"
                    host = "10.0.0.5"
                    security = "{value}"
                    {subtype}
                    "#,
                    site_passwd_line()
                ))
                .unwrap_err();
                let rendered = format!("{err:#}");
                assert!(rendered.contains("security"), "{value}: {rendered}");
                assert!(rendered.contains("rdp"), "the protocol that has it is named: {rendered}");
            }
        }
        // On RDP the key is read, and leaving it out is `auto` without being a
        // choice.
        let cfg = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "10.0.0.5"
            security = "tls"

            [[targets]]
            name = "bare"
            protocol = "rdp"
            host = "10.0.0.6"
            "#,
            site_passwd_line()
        ))
        .unwrap();
        assert_eq!(cfg.targets[0].security, Some(Security::Tls));
        assert_eq!(cfg.targets[1].security, None);
        assert_eq!(cfg.targets[1].security(), Security::Auto);
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

    /// The camera follows audio's rule: MS-RDPECAM is an RDP channel, so the key
    /// is refused on VNC at parse time and opt-in (default off) on RDP.
    #[test]
    fn camera_belongs_to_rdp_and_is_refused_on_vnc() {
        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "nope"
            protocol = "vnc"
            host = "10.0.0.5"
            camera = true
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("camera"), "{rendered}");
        assert!(rendered.contains("rdp"), "the protocol that does carry it is named: {rendered}");

        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "10.0.0.5"
            camera = true

            [[targets]]
            name = "quiet"
            protocol = "rdp"
            host = "10.0.0.6"
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].camera);
        assert!(!config.targets[1].camera, "the camera is opt-in");
    }

    /// The microphone follows the camera's rule: MS-RDPEAI is an RDP channel, so
    /// the key is refused on VNC at parse time and opt-in (default off) on RDP.
    #[test]
    fn microphone_belongs_to_rdp_and_is_refused_on_vnc() {
        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "nope"
            protocol = "vnc"
            host = "10.0.0.5"
            microphone = true
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("microphone"), "{rendered}");
        assert!(rendered.contains("rdp"), "the protocol that does carry it is named: {rendered}");

        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            host = "10.0.0.5"
            microphone = true

            [[targets]]
            name = "quiet"
            protocol = "rdp"
            host = "10.0.0.6"
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].microphone);
        assert!(!config.targets[1].microphone, "the microphone is opt-in");
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
            RenderPlan::Video { quality: 80, adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN), chroma: Chroma::Subsampled }
        );
        assert_eq!(plan.describe(), "video q80 · adaptive ≥20");

        let cfg = parse_target(
            "render_subtype = \"jpeg\"\n\
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
            RenderPlan::Video { quality: 80, adaptive: None, chroma: Chroma::Subsampled }
        );
    }

    /// Lossless PNG tiles — the default plan — have no dial for the link to move.
    #[test]
    fn render_adaptive_on_lossless_tiles_is_refused() {
        let err = parse_target("render_adaptive = true").unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("render_adaptive"), "{rendered}");
        assert!(rendered.contains("lossless"), "{rendered}");
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
