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

use crate::audio::PcmFormat;
#[cfg(all(feature = "embedded-gateway", unix))]
use crate::auth::EmbeddedToken;
use crate::auth::{GatewayAuth, SitePasswd};
use crate::protocol::HostDisplay;
use crate::throughput::MeterConfig;

/// Remote-desktop protocol of a target. Each has a server-side engine feeding
/// the same browser protocol (docs/architecture.md): `rdp` via the built-in RDP
/// client (src/rdp.rs over src/rdp_client), `vnc` via the built-in RFB client (src/vnc.rs). A Mac is reached
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
/// business; both current subtypes are `vnc`'s, and both describe the same Mac.
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
    /// Apple's native pasteboard is available, and the rectangles are zlib.
    Ard,
    /// The same Mac over Apple's own protocol revision, RFB 003.889: an
    /// AES-128-CBC record layer (see [`crate::vnc_record`]) carrying Apple's
    /// control messages (see [`crate::vnc_apple`]).
    ///
    /// Alone among the subtypes, none of this is documented by
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

/// How a target's pixels travel — the *transport*, the first of the two render
/// axes. The second, [`RenderSubtype`], is the codec of the base tiles, and a
/// lossy base also reads [`TargetConfig::image_quality`]. Two flat sibling keys
/// rather than a nested table, matching the rest of the target schema.
///
/// The two axes are orthogonal on purpose: this one says *what kind of thing
/// goes on the wire* (independent still tiles, or one video stream), the subtype
/// says *what a base tile is encoded as* (lossless PNG, a fixed-quality WebP, or
/// the classifier's per-tile choice between the two). The tiles transport takes
/// every subtype; the stream takes none.
///
/// Motion is deliberately *not* a value here. It changes nothing about what a
/// tile is or how one travels — it adds a second, cheaper encode for whatever is
/// moving right now, on top of the base tiles a target already sends. That is a
/// switch on the tiles transport ([`TargetConfig::render_motion`]), not a third
/// transport.
///
/// Only implemented transports are variants; anything else is refused by serde
/// with the list of what is accepted. See docs/architecture.md for the dial.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderType {
    /// Every changed region as an independent still image at the base codec.
    /// The default; with the default subtype (lossless PNG) and no
    /// [`TargetConfig::render_motion`] an unset target is byte-identical to the
    /// PNG-only gateway that preceded the dial.
    #[default]
    Tiles,
    /// The whole desktop as one video stream, at a fixed quality
    /// ([`TargetConfig::video_quality`]).
    ///
    /// Not a codec on the [`RenderSubtype`] axis, and deliberately not: those are
    /// all *per-tile* codecs, where every tile is independent, reorderable,
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

/// How much colour a video stream carries per pixel, as the encoder and the wire have
/// it: one of two VP9 profiles, and never a question. What a *target* asks for is
/// [`ChromaChoice`], which has a third answer this deliberately does not.
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
///
/// `Deserialize` for the session socket's `chroma` query parameter — the browser
/// naming the most colour its decoder takes, which is a settled chroma and not a
/// choice. No `Default`: a stream's chroma is resolved from a [`ChromaChoice`] and,
/// where that is [`ChromaChoice::Auto`], from that answer; there is no third source
/// for one to come from silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Chroma {
    /// 4:2:0 — one colour sample per 2×2 pixels, VP9 profile 0. The one every VP9
    /// decoder takes, hardware ones included, which is why it is what
    /// [`ChromaChoice::Auto`] falls back to.
    #[serde(rename = "420")]
    Subsampled,
    /// 4:4:4 — a colour sample per pixel, VP9 profile 1. On the picture above:
    /// a keyframe a third larger, inter frames no larger, a third more encode
    /// time, and coloured text that is the colour it was.
    ///
    /// The trade is the decoder. No hardware VP9 decoder takes profile 1, so this
    /// always decodes in software — Chromium does (measured headless, 2026-09-01),
    /// and a browser with no software VP9 at all, which is iOS and iPadOS, refuses
    /// the stream by name at `VideoDecoder.configure`, the same way it would refuse
    /// any configuration it lacks. Losing the hardware path is a smaller loss than
    /// it reads: the GPU-process decoder is the one that goes quiet under churn, and
    /// software libvpx is what answers every chunk (see
    /// `frontend/src/videoDecoder.ts`).
    ///
    /// [`ChromaChoice::Auto`] exists to get in front of that refusal without making
    /// the operator maintain a second target for the browsers that would raise it.
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

    /// How a card spells it, for a reader rather than for a key — the sampling
    /// itself, which is what says this is a chroma and not another quality dial.
    /// See [`RenderPlan::describe`].
    pub fn card_name(self) -> &'static str {
        match self {
            Self::Subsampled => "4:2:0",
            Self::Full => "4:4:4",
        }
    }
}

/// What [`TargetConfig::render_chroma`] can say: let the browser pick the profile,
/// or select one for every browser alike.
///
/// A type of its own rather than `Option<Chroma>`, because an encoder must never be
/// handed a chroma that still has a question in it: what a target asks for and what a
/// stream carries are different types, and [`TargetConfig::render_plan`] is the one
/// place the first becomes the second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum ChromaChoice {
    /// 4:4:4 where the browser's decoder takes VP9 profile 1, 4:2:0 where it says it
    /// does not. The default, and the only value that is not a decision about every
    /// browser at once.
    ///
    /// The browser is asked once, at page load, and states the answer on its session
    /// socket (`frontend/src/videoChroma.ts`, [`crate::ws`]); the gateway selects on
    /// it and never refuses a client for it, so a browser that answers wrongly still
    /// ends where it always did, at its own decoder's refusal by name. One target
    /// serves a desktop and an iPad without being written down twice, which is why
    /// this is the answer a target that says nothing gets.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// 4:2:0 for every browser ([`Chroma::Subsampled`]), selected rather than
    /// resolved: the decoder that would have taken profile 1 is sent the subsampled
    /// stream anyway. What every stream was before this key existed, and what to
    /// write to hold a fleet to the hardware-decodable bitstream.
    #[serde(rename = "420")]
    Subsampled,
    /// 4:4:4 for every browser ([`Chroma::Full`]), refusals included: an iPhone or
    /// iPad watching this target is sent a stream its `VideoDecoder` rejects by name.
    /// The setting to hold a fleet to one bitstream, or to pin one side of a
    /// comparison — [`Self::Auto`] is what serves a mixed one.
    #[serde(rename = "444")]
    Full,
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
/// paired with [`RenderType`]. Under [`RenderType::Tiles`] that is every tile,
/// or — with [`TargetConfig::render_motion`] — every tile except the ones
/// currently in motion, which a stream carries instead. [`RenderType::Video`]
/// sends no tiles and refuses the axis. All implemented codecs are
/// variants; serde refuses anything else.
///
/// Lossless is PNG and only PNG, and lossy is WebP and only WebP. The choice this
/// axis offers is how much of the screen each carries: [`Self::Webp`] for every
/// tile, or [`Self::Classify`] to spend it only where a picture wants it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderSubtype {
    /// Lossless PNG. The default.
    #[default]
    Png,
    /// WebP at [`TargetConfig::image_quality`]. Every tile goes to WebP — there
    /// is no content classifier — so flat UI and text soften along with
    /// photographic content. That is the trade the fixed dial makes.
    Webp,
    /// Per tile, whichever fits: a picture classifier ([`crate::classify`]) reads
    /// each tile's pixels and sends photographic content as WebP at
    /// [`TargetConfig::image_quality`] and everything else — flat UI, text — as
    /// lossless PNG. The classifier has no dial of its own;
    /// under [`TargetConfig::render_motion`] it is the base, so a settled cell is
    /// classified and a moving one takes the motion encode as usual.
    Classify,
}

impl RenderSubtype {
    /// How the config key spells it, for messages that name it back.
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Classify => "classify",
        }
    }
}

/// The tile encoder an engine uses, resolved from a target's render dial by
/// [`TargetConfig::render_plan`]. The axes and the qualities collapse to this, so
/// `rdp::run` / `vnc::run` and [`crate::encode::TileSink`] match on one value and
/// never touch the config enums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileCodec {
    /// Lossless PNG — the default path, and the only lossless one there is.
    Png,
    /// Every tile through WebP, at the dial's quality (1–100).
    Webp { quality: u8 },
    /// Per tile, whichever [`crate::classify`] says fits: photographic content
    /// through WebP at `quality`, everything else PNG. The decision runs on the
    /// encode worker, from the tile's own pixels, so it costs the read loops
    /// nothing.
    Classify {
        quality: u8,
        /// Outline the tiles the classifier sent lossy, in the pixels
        /// themselves, so QA reads the decision off the screen
        /// ([`TargetConfig::render_classify_debug`]). Carried here because the
        /// encoder is the one place the decision exists to be drawn.
        debug: bool,
    },
}

/// What [`TargetConfig::render_motion`] does with what it finds moving, resolved
/// from [`TargetConfig::video_quality`] and [`TargetConfig::render_chroma`].
///
/// A video stream per coalesced moving region — the one thing a moving region can
/// be, and not a codec choice: a still per cell is re-encoded from scratch every
/// frame, where this is an inter-frame stream, which is what moving content is cheap
/// in. What it costs instead is statefulness — an access unit means nothing out of
/// sequence — and that is why it never reaches the client as a tile. See
/// [`crate::regions`] for which regions get a stream and when one ends, and
/// [`crate::protocol::VideoUnit`] for what arrives.
///
/// A type of its own rather than two fields on [`RenderPlan::Tiles`], because
/// `Option<MotionEncode>` is the switch that keeps the whole motion path off and
/// there is no reading of a bare quality that says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MotionEncode {
    /// The 1–100 dial rather than a quantizer: turning that into one is
    /// [`crate::vp9`]'s business, and it is the only module that should know what a
    /// quantizer is.
    pub quality: u8,
    /// The floor of the adaptive quality walk — see [`RenderPlan::Video`]'s field
    /// of the same name, which this is: one link, one walk, whichever shape the
    /// stream has.
    pub adaptive: Option<u8>,
    /// [`TargetConfig::render_chroma`], resolved.
    pub chroma: Chroma,
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
        /// The stream a region changing fast is carried by instead, while it keeps
        /// changing.
        ///
        /// `None` is the switch that keeps the entire motion path off, and it is
        /// `None` for every configuration but `motion` — so a target that does not
        /// ask for the feature does not pay for it and is byte-identical to what it
        /// sent before the feature existed.
        motion: Option<MotionEncode>,
        /// Draw the motion path's decisions into the pixels. QA only, and only
        /// meaningful when `motion` is `Some`.
        debug: bool,
    },
    /// The whole framebuffer as one video stream at a fixed quantizer.
    ///
    /// The quality is the 1–100 dial rather than a quantizer: turning that into one is
    /// [`crate::vp9`]'s business, and it is the only module that should know what a
    /// quantizer is.
    Video {
        quality: u8,
        /// The floor of the adaptive quality walk, when
        /// [`TargetConfig::render_adaptive`] asked for one. `None` keeps the
        /// congestion walk's historical shape: pressure-only, floored at 1.
        ///
        /// A stream's alone, here and on [`MotionEncode`]. A still has no walk: it
        /// is sent once, so one sent coarse stays coarse until its pixels change,
        /// where a stream that fell below its dial is sharpened by its own next
        /// frame or by the cleanup that follows it.
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
        self.card(None)
    }

    /// [`Self::describe`] with the chroma slot said differently — the one thing a
    /// reader without a browser knows better than a resolved plan does, because
    /// [`ChromaChoice::Auto`] has nothing here to resolve against. See
    /// [`TargetConfig::render_summary`], the only caller that passes anything.
    fn card(&self, chroma_slot: Option<&str>) -> String {
        fn tile(codec: TileCodec) -> String {
            match codec {
                TileCodec::Png => "lossless png".to_owned(),
                TileCodec::Webp { quality } => format!("webp q{quality}"),
                TileCodec::Classify { quality, debug } => {
                    let debug = if debug { " (debug outlines)" } else { "" };
                    format!("classified png / webp q{quality}{debug}")
                }
            }
        }
        // The floor as a suffix on the stream it belongs to: the quality named
        // before it is a ceiling the link may fall below, and this is how far.
        fn floor(adaptive: Option<u8>) -> String {
            adaptive.map_or_else(String::new, |floor| format!(" · adaptive ≥{floor}"))
        }
        // Always named, because with `auto` the default there is no chroma a card
        // may leave unsaid: an unnamed one would read as 4:2:0 selected on a
        // session that is 4:2:0 only because this browser declined profile 1. What
        // the slot says is the profile on the wire — or, for a config card,
        // whatever `chroma_slot` puts there instead.
        let chroma = |chroma: Chroma| match chroma_slot {
            Some(slot) => format!(" {slot}"),
            None => format!(" {}", chroma.card_name()),
        };
        match self {
            RenderPlan::Video { quality, adaptive, chroma: c } => {
                format!("video q{quality}{}{}", chroma(*c), floor(*adaptive))
            }
            RenderPlan::Tiles { base, motion: None, .. } => {
                // No motion arm at all — plain `tiles`, whatever the base: whether
                // it is lossless is what the base already says.
                format!("tiles · {}", tile(*base))
            }
            RenderPlan::Tiles { base, motion: Some(motion), debug } => {
                let MotionEncode { quality, adaptive, chroma: c } = motion;
                let moving = format!("stream q{quality}{}{}", chroma(*c), floor(*adaptive));
                let debug = if *debug { " (debug outlines)" } else { "" };
                format!("motion · base {}, moving {moving}{debug}", tile(*base))
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
    /// (the account wlshare runs as, wayvnc's `enable_auth` username, a RealVNC
    /// system account — see [`crate::vnc_rsa_aes`]); RFB `VncAuth` cannot carry
    /// a name, so a plain target that sets it must set [`Self::password`] with
    /// it.
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
    /// considers right.
    ///
    /// How the pin is spent depends on the engine, because a pin is an opening
    /// size and each has its own way of stating one: RDP connects at it, High
    /// Performance creates its virtual display at it, and a generic VNC server
    /// is asked for it with one `SetDesktopSize`, as soon as it declares support
    /// (`Flags::pinned` in src/vnc.rs). Independent of [`Self::resize`], which
    /// governs whether the *window* drives the size afterwards: without it the
    /// session stays at the pin, and with it RDP and High Performance open at
    /// the pin and then follow the browser, because both state a size at connect
    /// and no report can precede that. Generic VNC cannot state one until the
    /// server declares support, by which time a `resize` browser has already
    /// reported its window and superseded the pin, so such a session opens at
    /// the window and the pin is left answering `DefaultSize`. Standard `ard` is
    /// the exception with nothing to spend a pin on — it exposes the Mac's
    /// physical displays, which this gateway never resizes — and there the keys
    /// only answer a later default-size request.
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
    /// Allow client-driven resize: hand this target's desktop size to the
    /// client's window. A desktop client reports every window change while this
    /// is on; there is no client-side mode, manual resize command, or second
    /// config key.
    ///
    /// On RDP this also turns on density matching, because there a density *is* a
    /// resize: the Display Control channel this negotiates is the only way to tell
    /// a live session to render at 200%, so a Retina client gets twice the pixels
    /// and a UI drawn twice as large. Off, an RDP target ignores the client's
    /// density entirely. An RDP resize is the graphics pipeline's, so the key is
    /// refused beside `egfx = false`.
    ///
    /// On `ard-high-performance` the setup descriptor always enables the Mac's
    /// dynamic geometry; this flag decides only whether the window keeps
    /// driving it after the open. Standard `ard` refuses the option because it
    /// exposes physical displays.
    #[serde(default)]
    pub resize: bool,
    /// RDP's graphics pipeline (MS-RDPEGFX), on by default. On, a Windows host
    /// draws the desktop through the pipeline's surfaces and marks every frame,
    /// and a resize is a graphics reset. Off, the host draws with bitmap updates
    /// and the desktop keeps its opening size — [`Self::resize`] is refused
    /// beside it; that is the escape hatch for a host whose pipeline this
    /// client's decoders cannot yet paint, and the path every non-Windows server
    /// takes regardless.
    ///
    /// `Option` rather than a bare default so that setting it on a VNC target,
    /// which has no graphics pipeline to switch, is refused at parse time
    /// instead of accepted and left inert; `None` reads as on
    /// ([`TargetConfig::egfx`]).
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
    /// The `audio` key as written, which [`ConfigFile::parse`] resolves into
    /// [`Self::audio`]. Refused on either Apple subtype, whose sound the
    /// `[airplay]` table decides.
    #[serde(default, rename = "audio")]
    pub audio_key: Option<bool>,
    /// Carry the remote's sound. Packets are sent only while the attached client
    /// subscribes. RDP negotiates it at connect (MS-RDPEA); a plain `vnc` target
    /// asks a generic server for wlshare's audio extension, FLAC on the RFB
    /// connection, and is answered by wlshare — see [`crate::vnc_audio`]. Both
    /// opt in with `audio = true`. Either Apple subtype carries it exactly when
    /// the gateway-wide `[airplay]` table is set: the Mac sends its sound to the
    /// gateway's AirPlay speaker — see [`crate::airplay`].
    #[serde(skip)]
    pub audio: bool,
    /// Which codec [`Self::audio`] encodes with; `None` reads as
    /// [`AudioCodec::Opus`]. `Option` rather than a bare default so that setting
    /// it on a target that never enabled audio is refused at parse time instead
    /// of accepted and left inert.
    #[serde(default)]
    pub audio_codec: Option<AudioCodec>,
    /// Offer the remote a redirected camera: MS-RDPECAM on RDP, and on a generic
    /// VNC target the wlshare camera extension ([`crate::vnc_camera`]), which is
    /// asked for the way [`Self::audio`]'s extension is — a server that never
    /// announces it leaves the camera unplugged. Rejected on both Apple
    /// subtypes: Screen Sharing speaks no such extension.
    ///
    /// **Experimental**, for lack of tests. The socket's session rules and its
    /// message encodings are unit tested, and so are both wires; the RDP
    /// redirection itself is exercised only against a real Windows host, because
    /// only a host that creates the `RDCamera_Device_Enumerator` channel — a
    /// workstation, or a Windows Server carrying the Remote Desktop Session Host
    /// role — has anywhere to redirect a camera to, and the container dummies
    /// are neither.
    ///
    /// Capability only. The device itself appears when a client enables the
    /// camera — explicitly, per session, never remembered — by opening
    /// `/ws/camera`; a target with this key and no such client offers the
    /// remote nothing. The browser encodes H.264 and the gateway passes it
    /// through, so there is no codec key beside this one.
    #[serde(default)]
    pub camera: bool,
    /// Offer the remote this browser's microphone: MS-RDPEAI on RDP, and on a generic VNC
    /// target the wlshare microphone extension ([`crate::vnc_mic`]), asked for the way
    /// [`Self::camera`]'s is — a server that never announces it leaves the microphone
    /// unplugged. Rejected on both Apple subtypes: Screen Sharing speaks no such
    /// extension.
    ///
    /// Capability only, like [`Self::camera`]: the recording device is fed when a client
    /// enables its microphone — explicitly, per session — by opening `/ws/mic`. The
    /// browser sends speech-grade Opus and the gateway decodes it to the PCM the host
    /// records in, so there is no codec or quality key beside this one.
    #[serde(default)]
    pub microphone: bool,
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
    /// Render *transport* for this target. Defaults to [`RenderType::Tiles`],
    /// which with the default subtype (lossless PNG) and no [`Self::render_motion`]
    /// is byte-identical to before the dial existed. Validated against
    /// [`Self::render_subtype`] and [`Self::image_quality`] in
    /// [`ConfigFile::parse_with`]. Works for both RDP and VNC.
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
    /// The quality (1–100) of every lossy still image this target sends: what
    /// [`RenderSubtype::Webp`] encodes every tile at, and what
    /// [`RenderSubtype::Classify`] encodes its photographic tiles at.
    /// Required exactly when the subtype is one of those two and refused for
    /// [`RenderSubtype::Png`], which is lossless and has no dial. `None`
    /// (unset) is the default.
    ///
    /// Under [`Self::render_motion`] this is the *base* quality — what a settled
    /// cell gets — and it is omitted when the base is lossless PNG. What is moving
    /// has the other dial, [`Self::video_quality`].
    #[serde(default)]
    pub image_quality: Option<u8>,
    /// The quality (1–100) a VP9 stream holds on a link that can carry it — the
    /// whole desktop under [`RenderType::Video`], or one coalesced moving region
    /// under [`Self::render_motion`]. `None` reads as [`DEFAULT_VIDEO_QUALITY`];
    /// refused on a target that streams neither way, since nothing else this
    /// gateway sends is a stream.
    ///
    /// One key for both because it is one dial: the two reach the same encoder
    /// through the same congestion walk, floored by the same
    /// [`Self::render_adaptive_min`], and a target can never have both — motion is
    /// a discount on settled tiles and `video` has none. Two names for a slot with
    /// one occupant is a distinction the reader has to keep and the code never made.
    ///
    /// A ceiling rather than a promise: a link that cannot hold it coarsens until
    /// it can, and one with room to spare never earns better. Under
    /// [`Self::render_motion`] it can go very low — motion hides the artifacts and
    /// a region that stops moving is re-sent at the base encode anyway.
    #[serde(default)]
    pub video_quality: Option<u8>,
    /// Hand the cells changing fastest right now to a second and much cheaper
    /// encode, on top of the base tiles this target already sends.
    ///
    /// A switch on [`RenderType::Tiles`] rather than a transport of its own,
    /// because it *builds on* the base rather than replacing it: a settled cell is
    /// still sent as [`Self::render_subtype`] at [`Self::image_quality`], and a
    /// cell that stops changing is re-sent once at that base encode, so a paused
    /// screen returns to full quality on its own. The base is the truth;
    /// motion is a temporary discount on what is too busy to notice.
    ///
    /// What a moving region gets instead is a video stream per coalesced region
    /// ([`MotionEncode`]), at [`Self::video_quality`] — which this key gives a
    /// meaning to rather than demanding: unset, the stream runs at
    /// [`DEFAULT_VIDEO_QUALITY`] like any other — and [`Self::render_chroma`].
    /// Refused with [`RenderType::Video`], which streams the whole desktop and has
    /// nothing left to discount.
    #[serde(default)]
    pub render_motion: bool,
    /// Outline every piece the motion path emits, in the pixels themselves, so
    /// what the detection decided is visible on the screen instead of inferred from
    /// how blurry something looks. A QA aid for [`Self::render_motion`] and refused
    /// without it; off unless asked for.
    ///
    /// See [`crate::encode::TileSink::damage`] for what the colours mean. The marks
    /// go on the *copy* handed to the encoder — on the crop a region stream encodes
    /// rather than on the mirror — so the shadow and the mirror keep the true pixels,
    /// and a cleanup erases the outline it replaces.
    #[serde(default)]
    pub render_motion_debug: bool,
    /// Chroma sampling of this target's video streams — `render_type = "video"`
    /// and `render_motion = true` alike; `None` reads as [`ChromaChoice::Auto`],
    /// which is every browser getting the most colour its own decoder takes.
    ///
    /// Written down only to take that decision away from the browser: `"444"` sends
    /// profile 1 to a decoder that refuses it by name, `"420"` sends the subsampled
    /// stream to one that would have taken the colour. Both are the right key for a
    /// measurement or for a fleet held to one bitstream, and the wrong one for a
    /// target watched from more than one kind of browser. See [`ChromaChoice`] and
    /// [`Self::render_plan`].
    ///
    /// `Option` rather than a bare default so that setting it on a target that
    /// streams nothing is refused at parse time instead of accepted and left inert,
    /// the same rule as `audio_codec` without `audio`.
    #[serde(default)]
    pub render_chroma: Option<ChromaChoice>,
    /// Outline every tile the classifier sends lossy, in the pixels
    /// themselves, so which regions it reads as photographic is visible on the
    /// screen instead of inferred from how soft something looks. A QA aid for
    /// [`RenderSubtype::Classify`] and refused for any other subtype; off
    /// unless asked for.
    ///
    /// The outline goes on the copy handed to the lossy encoder, never on the
    /// pixels the shadow records as delivered — so the mark lasts exactly as
    /// long as the lossy tile it describes, and the next change repaints it
    /// away. PNG tiles are never marked: unmarked-and-sharp is the quiet
    /// majority, and outlining it would say nothing.
    #[serde(default)]
    pub render_classify_debug: bool,
    /// Draw the tile lattice over the desktop as dashed lines, so where the
    /// gateway cuts damage is something the screen shows rather than something the
    /// reader works out from a constant. A QA aid for the transport that sends
    /// tiles at all, [`RenderType::Tiles`], and refused for [`RenderType::Video`],
    /// which sends none; off unless asked for.
    ///
    /// Unlike the other two debug keys nothing is painted into the pixels: the
    /// lattice is fixed to the framebuffer and the pixels are not, so the client
    /// draws it over the canvas from the pitch
    /// [`crate::protocol::ServerMsg::Connected`] carries. See that field for why
    /// a lattice encoded into tiles would not survive a scroll.
    #[serde(default)]
    pub render_grid_debug: bool,
    /// Let [`Self::video_quality`] track the measured link — on unless the
    /// operator turned it off.
    ///
    /// The configured quality stays the *ceiling* — a link with room to spare
    /// never earns a better picture than the one asked for — and the walk's floor
    /// is [`Self::render_adaptive_min`]. A video stream (`render_type = "video"`,
    /// or `render_motion = true`) already gives quality up when queueing a frame
    /// blocks; this adds the client's own lag — how long the oldest unacknowledged
    /// paint batch has been owed, beyond the link's measured floor — as a second
    /// reason to, and moves the walk's floor up from 1.
    ///
    /// The streams' key alone, and refused on a target that has none.
    /// [`Self::image_quality`] never moves: a still is sent once, so a tile the
    /// link coarsened would keep that picture until its pixels next changed, and
    /// coming back for it costs a second encode of something that was not going
    /// to be sent again. A stream pays nothing like it — its next frame sharpens
    /// it, and a region that stops is owed a cleanup whatever quality it ran at.
    ///
    /// `Option` rather than a bare `bool` so that `false` on a target that streams
    /// nothing is refused the same way `true` is: neither would be read, and a key
    /// that was written names an expectation. Resolved by the accessor of the same
    /// name.
    #[serde(default)]
    pub render_adaptive: Option<bool>,
    /// Floor (1–100) for [`Self::render_adaptive`]; `None` reads as
    /// [`DEFAULT_RENDER_ADAPTIVE_MIN`], or as [`Self::video_quality`] where the dial
    /// sits below it — a default floor never narrows a stream's walk to nothing.
    /// Must not exceed [`Self::video_quality`] when written —
    /// a floor above the ceiling is a contradiction better refused than resolved.
    /// Refused beside `render_adaptive = false`, and on a target that streams
    /// nothing.
    #[serde(default)]
    pub render_adaptive_min: Option<u8>,
}

/// The quality floor [`TargetConfig::render_adaptive`] falls back to when
/// [`TargetConfig::render_adaptive_min`] is unset. Low enough to matter on a
/// struggling link, high enough that text stays legible.
pub const DEFAULT_RENDER_ADAPTIVE_MIN: u8 = 20;

/// The stream quality a target streams at when [`TargetConfig::video_quality`] is
/// unset — the ceiling of the adaptive walk, not a promise. High enough that a link
/// with room to spare shows a desktop worth looking at, and the walk is what takes
/// it down on one that has not.
pub const DEFAULT_VIDEO_QUALITY: u8 = 90;

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

    /// RDP's graphics pipeline switch, on unless the operator turned it off.
    pub fn egfx(&self) -> bool {
        self.egfx.unwrap_or(true)
    }

    /// [`Self::render_subtype`] resolved: lossless PNG unless the operator chose.
    pub fn render_subtype(&self) -> RenderSubtype {
        self.render_subtype.unwrap_or_default()
    }

    /// The ceiling every VP9 stream of this target holds to, resolved: what the
    /// operator wrote, else [`DEFAULT_VIDEO_QUALITY`]. Only a target that
    /// [`Self::streams_video`] has one to read — parse refuses the key on any
    /// other.
    pub fn video_quality(&self) -> u8 {
        self.video_quality.unwrap_or(DEFAULT_VIDEO_QUALITY)
    }

    /// The adaptive walk, resolved: on unless the operator turned it off.
    pub fn render_adaptive(&self) -> bool {
        self.render_adaptive.unwrap_or(true)
    }

    /// The tile encoders to use for this target. This is the whole of the render
    /// dial as the engines see it: the axes and the qualities collapse to one
    /// [`RenderPlan`], so `rdp::run` / `vnc::run` need not know the config enums.
    ///
    /// A lossy base codec carries its quality, which [`ConfigFile::parse_with`] has
    /// already guaranteed is present and in range; the `None` arm falls back to the
    /// safe answer — lossless PNG — rather than trusting that here. A stream's
    /// quality has a default instead ([`Self::video_quality`]), so the two stream
    /// shapes read it off the switch that put them there.
    ///
    /// `decoder` is the most colour the attached browser said its `VideoDecoder`
    /// takes, carried on the session socket and held with its attachment
    /// ([`crate::session::SessionManager::attach`]). It is read by
    /// [`ChromaChoice::Auto`] and by nothing else: a target that names a profile
    /// gets that profile whatever this says, which is what keeps the explicit key a
    /// decision no browser can overrule, and a target that streams nothing reads it
    /// not at all.
    pub fn render_plan(&self, decoder: Chroma) -> RenderPlan {
        let quality = self.video_quality();
        // The floor the walk will hold to, which is never above the ceiling it walks
        // under. Only the *default* floor can sit there — an explicit
        // `render_adaptive_min` over a configured `video_quality` is refused at parse —
        // and a target that asked for a walk gets the widest one its dial admits. Held
        // here rather than left to the encoder so that a card cannot state a floor the
        // stream never walks down to.
        let adaptive = self.render_adaptive().then(|| {
            self.render_adaptive_min
                .unwrap_or(DEFAULT_RENDER_ADAPTIVE_MIN)
                .min(quality)
        });
        let chroma = match self.render_chroma.unwrap_or_default() {
            ChromaChoice::Subsampled => Chroma::Subsampled,
            ChromaChoice::Full => Chroma::Full,
            ChromaChoice::Auto => decoder,
        };
        if self.render_type == RenderType::Video {
            return RenderPlan::Video { quality, adaptive, chroma };
        }
        let base = match (self.render_subtype(), self.image_quality) {
            (RenderSubtype::Webp, Some(quality)) => TileCodec::Webp { quality },
            (RenderSubtype::Classify, Some(quality)) => {
                TileCodec::Classify { quality, debug: self.render_classify_debug }
            }
            _ => TileCodec::Png,
        };
        let motion = self.render_motion.then_some(MotionEncode { quality, adaptive, chroma });
        RenderPlan::Tiles { base, motion, debug: self.render_motion_debug }
    }

    /// The render dial for a reader with no browser in front of it — the TUI's
    /// target card, which describes a config file rather than a session.
    ///
    /// The chroma is where a config card and a session card part: a session has a
    /// browser and therefore a profile, and a file has only what the operator asked
    /// for. So this card says `chroma auto` where the browser decides — which is the
    /// default, and now most targets — and names the sampling where one was
    /// selected for every browser alike. Naming one of `auto`'s two answers here
    /// would print a colour this target may never send.
    ///
    /// The decoder passed below is read by [`ChromaChoice::Auto`] and by nothing
    /// else, and `auto` is exactly the case whose slot is overwritten — so the
    /// argument reaches no card, and a selected `"420"` or `"444"` prints itself.
    pub fn render_summary(&self) -> String {
        let slot = match self.render_chroma.unwrap_or_default() {
            ChromaChoice::Auto => Some("chroma auto"),
            ChromaChoice::Subsampled | ChromaChoice::Full => None,
        };
        self.render_plan(Chroma::Subsampled).card(slot)
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

    /// The one PCM format this target's wave buffers can be in, known before the
    /// remote has said anything: what the RDP engine asks a server to redirect
    /// ([`crate::audio::PCM_CD_QUALITY`]), what AirPlay carries from a Mac (the
    /// same), or what a generic VNC server
    /// is asked to send over wlshare's audio extension
    /// ([`crate::vnc_audio::SOURCE_FORMAT`]) — the last of which this client
    /// chooses outright, since the extension leaves the format to the client. The
    /// session builds its encoder from this when the audio socket opens before the
    /// remote's channel is up, so it has to be the source's — an encoder built for
    /// the wrong rate plays every note at the wrong pitch. Callers gate on
    /// [`Self::audio`], as with [`Self::audio_plan`].
    pub fn audio_source_format(&self) -> PcmFormat {
        match self.protocol {
            Protocol::Rdp => crate::audio::PCM_CD_QUALITY,
            Protocol::Vnc if self.receives_airplay() => crate::audio::PCM_CD_QUALITY,
            Protocol::Vnc => crate::vnc_audio::SOURCE_FORMAT,
        }
    }

    /// Whether this target's sound, when it has any, arrives at the gateway's AirPlay
    /// speaker rather than over its own connection: either Apple subtype.
    pub fn receives_airplay(&self) -> bool {
        match (self.protocol, self.subtype) {
            (Protocol::Vnc, Some(Subtype::Ard | Subtype::ArdHighPerformance)) => true,
            (Protocol::Vnc, None) | (Protocol::Rdp, _) => false,
        }
    }

    /// Whether this target puts moving pixels on the wire as a video stream — either the whole
    /// desktop (`render_type = "video"`) or a region at a time (`render_motion = true`).
    ///
    /// Answered off the render dial alone, without resolving a plan, because
    /// [`ConfigFile::parse_with`] asks it before it has validated the qualities a plan needs.
    /// The engines ask it too: a streaming target has every size it asks a remote for held
    /// under the stream's picture ceiling ([`crate::video::fit_ceiling`]), where a tiles
    /// target asks for the screen as it is.
    pub fn streams_video(&self) -> bool {
        match self.render_type {
            RenderType::Video => true,
            RenderType::Tiles => self.render_motion,
        }
    }
}

/// What a session opens at when neither the config nor the connecting client
/// named a size: no screen to measure, no operator to ask, one desk-shaped
/// answer.
///
/// Points, not backing pixels, and chosen for what it costs at 2x rather than
/// for the shape it makes at 1x: a HiDPI client renders this desktop at twice
/// the points, and 1920×1080 at 2x is 3840×2160 — 4K to capture, scale and
/// encode every frame, which is more than a gateway should take on for a
/// session nobody asked to be that large. 1440×900 is 2880×1800 at 2x,
/// five-eighths of the pixels, and the shape a working surface prefers besides.
///
/// It is also what a phone or tablet gets: a touch client asks for this rather
/// than its own screen, which is portrait and far too small to be a desktop
/// (`sendMobileSize` in `frontend/src/useRemoteDesktop.ts`). An operator who
/// wants the larger desk pins `width`/`height` and pays for it deliberately,
/// up to the ceiling a video stream encodes within
/// ([`crate::video::MAX_LONG_SIDE`]).
pub const DEFAULT_SIZE: (u16, u16) = (1440, 900);

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
    /// resolves against the process's working directory.
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
    /// The `[meter]` table: where the browser sockets' throughput is recorded. Absent,
    /// or present with `enabled = false`, records nothing; present it must say which.
    /// Top-level for [`Self::branding`]'s reason — an embedded config may set it too.
    #[serde(default)]
    pub meter: Option<MeterSection>,
    /// The `[airplay]` table: the password of the AirPlay speaker a Mac sends its
    /// sound to. Its presence turns audio on for every Apple target, and it is
    /// refused when no target has an Apple subtype.
    /// Top-level for [`Self::branding`]'s reason — an embedded config may set it too.
    #[serde(default)]
    pub airplay: Option<AirPlaySection>,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

/// The `[airplay]` table as written. See [`crate::airplay`].
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AirPlaySection {
    /// What a Mac is asked for when it picks the speaker, and remembers in its
    /// keychain after. Required: the speaker is on the whole LAN, and any Mac on it
    /// that knew no password could play into whichever session is running.
    /// Plaintext, because AirPlay's Digest challenge needs it to verify an answer.
    pub password: String,
}

/// The gateway's AirPlay speaker, resolved: what it is called and the password a
/// Mac is asked for. See [`crate::airplay`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AirPlayConfig {
    /// The speaker's name in a Mac's Sound menu: the gateway's branding, with
    /// ` - remotex` after it.
    pub name: String,
    pub password: String,
}

/// The `[meter]` table as written. See [`crate::throughput`].
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeterSection {
    /// Whether the throughput is recorded at all. Required, so a `[meter]` table says
    /// in as many words which it is: the other keys are the settings, checked either
    /// way, and this one alone decides whether anything is recorded. A table that
    /// leaves it out is refused rather than read as off — the settings under it are
    /// there to be used, and guessing which way an operator meant them is the one
    /// thing a config file should never do.
    pub enabled: bool,
    /// The SQLite database the records are kept in. Absent is `meter.sqlite3` in the
    /// gateway's state directory, and a relative path is taken from that directory too.
    pub database: Option<PathBuf>,
    /// Records kept per target and socket; the oldest go first. One record is one
    /// timeframe, whose length is the gateway's to decide, not this file's.
    #[serde(default = "default_meter_max_records")]
    pub max_records: usize,
}

/// A week of one-minute timeframes, for a socket busy all week.
fn default_meter_max_records() -> usize {
    10_080
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
    /// `[meter]`, resolved. `None` records nothing.
    pub meter: Option<MeterConfig>,
    /// The AirPlay speaker, when an Apple target carries audio; `None` starts none.
    pub airplay: Option<AirPlayConfig>,
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
        let airplay = config.airplay.is_some();
        for target in &mut config.targets {
            if target.port == 0 {
                target.port = target.protocol.default_port();
            }
            // A Mac's sound is the gateway's AirPlay speaker's, which is not the
            // target's to turn on or off: the `[airplay]` table is, for every Mac.
            anyhow::ensure!(
                !(target.receives_airplay() && target.audio_key.is_some()),
                "target {:?} sets audio on an {} target, whose sound arrives at the gateway's \
                 AirPlay speaker — the [airplay] table turns that on for every Mac. Remove \
                 the key.",
                target.name,
                target.subtype.map_or("apple", Subtype::name)
            );
            target.audio = if target.receives_airplay() {
                airplay
            } else {
                target.audio_key.unwrap_or(false)
            };
        }
        #[cfg(feature = "embedded-gateway")]
        if audience == Audience::Embedded {
            // Refused rather than ignored, and named as a whole block rather than
            // key by key: every one of them is a decision the launcher has already
            // made for this gateway — a private Unix socket under the instance and
            // a token instead of a login. A key that is quietly overridden is worse
            // than one that is refused: it reads as configuration and behaves as
            // decoration.
            anyhow::ensure!(
                config.server.is_none(),
                "an embedded instance config may not have a [server] block: \
                 the launcher decides where its gateway listens and how it \
                 authenticates. Only [branding] and [[targets]] belong here"
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
        if let Some(meter) = &config.meter {
            anyhow::ensure!(
                meter.database.as_ref().is_none_or(|database| !database.as_os_str().is_empty()),
                "[meter].database is empty — name the SQLite file, or leave the key out for \
                 meter.sqlite3 in the gateway's state directory"
            );
            anyhow::ensure!(meter.max_records >= 1, "[meter].max_records must be at least 1");
        }
        // Every Mac's sound arrives at the gateway's AirPlay speaker, which the
        // `[airplay]` table turns on and which asks every sender for its password:
        // the speaker answers the whole LAN. A table with no Mac to play through it
        // is refused rather than started.
        if let Some(airplay) = &config.airplay {
            anyhow::ensure!(
                cfg!(feature = "airplay"),
                "[airplay] is set, and this remotex was built without the airplay feature"
            );
            anyhow::ensure!(
                config.targets.iter().any(TargetConfig::receives_airplay),
                "[airplay] is set, and there is no ard or ard-high-performance target, so \
                 nothing would play through the speaker. Remove the table, or add a Mac"
            );
            anyhow::ensure!(
                !airplay.password.trim().is_empty(),
                "[airplay].password is empty — every Mac on the LAN could then play into the \
                 session. Set one"
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
                 target render_type = \"video\" or render_motion = true, or remove the \
                 key",
                target.name
            );
            // The graphics pipeline is RDP's alone: EGFX is an RDP channel, so on a
            // VNC target the key could only be a belief about the wrong protocol,
            // and either value would be silently inert.
            anyhow::ensure!(
                target.egfx.is_none() || target.protocol == Protocol::Rdp,
                "target {:?} sets egfx on a {} target, and only rdp has a graphics pipeline \
                 to switch. Remove the key.",
                target.name,
                target.protocol.name()
            );
            // An RDP resize is a graphics reset, which only the pipeline has: the
            // bitmap path keeps its opening size, so the pair is refused rather than
            // left to a channel whose layouts would go nowhere. MS-RDPEDISP's other
            // answer, a Deactivation-Reactivation Sequence, is left out on purpose: see
            // "Bitmap updates" in docs/rdp-client.md.
            anyhow::ensure!(
                !(target.protocol == Protocol::Rdp && target.resize && !target.egfx()),
                "target {:?} sets resize with egfx = false, and an rdp desktop is resized \
                 through the graphics pipeline alone. Remove one of the two keys.",
                target.name
            );
            // Audio is carried three ways: MS-RDPEA on RDP, wlshare's audio
            // extension on a generic VNC target ([`crate::vnc_audio`]), and the
            // gateway's AirPlay speaker for either Apple subtype, whose Screen
            // Sharing carries no sound a client can take ([`crate::airplay`]). The
            // last is checked with the `[airplay]` table above.
            //
            // A generic VNC target is *asked* rather than assumed: the extension is
            // discovered on the connection, and a server that never announces it —
            // wayvnc, TigerVNC, x11vnc — runs the session in silence. The key is
            // what makes this client ask at all.
            //
            // Everything downstream of the channel — the socket, the bridge, the
            // encoders — is protocol-agnostic, which is why this rule is about the
            // *engine* and not about any of them.
            // The camera rides MS-RDPECAM on RDP and wlshare's camera extension on a
            // generic VNC target, asked for the way its audio extension is: a
            // server that never announces it leaves the camera unplugged. Apple's
            // Screen Sharing speaks no such extension, in either subtype.
            anyhow::ensure!(
                !target.camera || target.protocol == Protocol::Rdp || target.subtype.is_none(),
                "target {:?} sets camera on an {} target, and Apple's Screen Sharing has nowhere \
                 to put one: the camera rides MS-RDPECAM on rdp and wlshare's camera extension on \
                 a generic vnc target. Remove the key.",
                target.name,
                target.subtype.map_or("apple", Subtype::name)
            );
            // The microphone likewise: MS-RDPEAI on RDP, wlshare's microphone extension on
            // a generic VNC target, and nothing on a Mac.
            anyhow::ensure!(
                !target.microphone || target.protocol == Protocol::Rdp || target.subtype.is_none(),
                "target {:?} sets microphone on an {} target, and Apple's Screen Sharing has \
                 nowhere to put one: the microphone rides MS-RDPEAI on rdp and wlshare's \
                 microphone extension on a generic vnc target. Remove the key.",
                target.name,
                target.subtype.map_or("apple", Subtype::name)
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
                // The client offers only NLA, and CredSSP has nothing to log on
                // with unless both are set.
                (Protocol::Rdp, None) => {
                    anyhow::ensure!(
                        !target.username.is_empty() && !target.password.is_empty(),
                        "target {:?} is protocol \"rdp\" and needs both username and password — \
                         this client logs on only through NLA, which carries the two together",
                        target.name
                    );
                }
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
            // Motion is a switch on the tiles transport, so the only transport it
            // can be off is `video` — which streams the whole desktop and has
            // nothing left to hand a cheaper encode to.
            anyhow::ensure!(
                !target.render_motion || target.render_type == RenderType::Tiles,
                "target {:?} sets render_motion with render_type = \"video\" — motion is a \
                 discount on the cells of a tiles target that are moving right now, and \
                 \"video\" sends no cells: it sends the whole desktop as one stream, already \
                 inter-frame throughout. Drop render_motion to keep \"video\", or set \
                 render_type = \"tiles\" to keep motion",
                target.name
            );
            // The overlay is the one key left that is motion's alone — the quality
            // it used to own is now `video_quality`, demanded below off
            // `streams_video`. A config that draws the outlines without the switch
            // has misunderstood which dial it is turning, more likely a
            // `render_motion` that was never written than a deliberate choice, so it
            // is worth saying so rather than silently ignoring it.
            anyhow::ensure!(
                target.render_motion || !target.render_motion_debug,
                "target {:?} sets render_motion_debug without render_motion — the outlines \
                 show which cells the motion path put in a stream, and without the switch \
                 there is no such decision to draw",
                target.name
            );
            // Two quality keys, one per kind of thing on the wire: `video_quality`
            // is the VP9 stream's dial and belongs to the two ways a target can have
            // one, `image_quality` is the base codec's and belongs to the transport
            // that has a base. A quality under the other name is the dial of
            // something this target does not send — most likely a config written for
            // a render dial it no longer has.
            if target.render_type == RenderType::Video {
                anyhow::ensure!(
                    target.image_quality.is_none(),
                    "target {:?} is render_type \"video\" and sets image_quality, which \
                     is the quality of the base tiles' codec — and \"video\" sends no \
                     tiles. video_quality is the stream's dial",
                    target.name
                );
            }
            // The render dial has two axes and they are validated together, because
            // only some pairings mean anything and `image_quality` belongs to exactly
            // one of them. The match is exhaustive so a future variant cannot be
            // added without deciding what it pairs with here.
            match (target.render_type, target.render_subtype) {
                (RenderType::Tiles, None | Some(RenderSubtype::Png)) => {
                    anyhow::ensure!(
                        target.image_quality.is_none(),
                        "target {:?} sets image_quality, which the lossless \"png\" base \
                         has no use for. Set render_subtype = \"webp\" for a fixed lossy \
                         quality, or \"classify\" to spend it only on photographic tiles \
                         — or, under render_motion, video_quality is the dial for the \
                         cells in motion",
                        target.name
                    );
                }
                // Both lossy bases make the same demand for the same reason:
                // `webp` spends the quality on every tile, `classify` only on the
                // ones its classifier reads as photographic, and neither has a
                // default — a quality nobody chose is not a quality.
                //
                // `render_motion` does not enter into it, and that is the point of
                // it being a switch rather than a transport: the base is the base
                // either way, and motion only changes which cells reach it. The
                // interesting configuration falls out of that on its own — a
                // lossless base with a lossy discount, where text and flat UI stay
                // perfect and only what moves gets ugly.
                (RenderType::Tiles, Some(RenderSubtype::Webp | RenderSubtype::Classify)) => {
                    let q = target.image_quality.with_context(|| format!(
                        "target {:?} sets a lossy render_subtype but no image_quality — it \
                         needs one, an integer 1–100",
                        target.name
                    ))?;
                    anyhow::ensure!(
                        (1..=100).contains(&q),
                        "target {:?} sets image_quality = {q}, which is out of range — it \
                         must be 1–100",
                        target.name
                    );
                }
                // `video` is the one transport with nothing on the subtype axis to
                // pair with. The other cuts damage into independent images and
                // chooses which codec to encode them with; this one is a single
                // stateful video stream carrying the whole framebuffer, so there is
                // no per-tile codec left to name.
                // Nothing to pair: the subtype axis is empty here by definition, and
                // this transport's quality is `video_quality`, demanded below with
                // the other way of carrying a stream.
                (RenderType::Video, None) => {}
                // Every value is refused here, `png` included: it is the default the
                // key would otherwise read as, but a key that was written names an
                // expectation, and on this transport nothing would ever read it.
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
                 \"classify\" — the outlines show which tiles the classifier sent lossy, \
                 and no other subtype makes that decision",
                target.name
            );
            // The lattice is the grid damage is cut at, so it means something under
            // the transport that cuts damage — and nothing under the one that does
            // not cut it at all.
            anyhow::ensure!(
                target.render_type != RenderType::Video || !target.render_grid_debug,
                "target {:?} sets render_grid_debug with render_type = \"video\" — the \
                 grid is the tile lattice, and \"video\" sends no tiles: it sends the whole \
                 desktop as one stream, where there is no boundary to draw. Set \
                 render_type = \"tiles\" to keep the grid",
                target.name
            );
            // The three stream keys ask one question: does this target put a VP9
            // stream on the wire at all. A target that streams gets the whole dial,
            // defaults included; one that streams nothing has nothing for any of the
            // three to describe, so each is refused there rather than left inert.
            // Asking `streams_video` rather than matching the two shapes is what
            // keeps that one rule one rule.
            if target.streams_video() {
                if let Some(q) = target.video_quality {
                    anyhow::ensure!(
                        (1..=100).contains(&q),
                        "target {:?} sets video_quality = {q}, which is out of range — it \
                         must be 1–100",
                        target.name
                    );
                }
                anyhow::ensure!(
                    target.render_adaptive_min.is_none() || target.render_adaptive(),
                    "target {:?} sets render_adaptive_min beside render_adaptive = false — \
                     the floor belongs to the adaptive walk, and without the walk nothing \
                     would read it",
                    target.name
                );
                if let Some(floor) = target.render_adaptive_min {
                    anyhow::ensure!(
                        (1..=100).contains(&floor),
                        "target {:?} sets render_adaptive_min = {floor}, which is out of \
                         range — it must be 1–100",
                        target.name
                    );
                    // A floor above the ceiling is a contradiction, and the stream's
                    // quality is the ceiling the walk must fit under.
                    let ceiling = target.video_quality();
                    anyhow::ensure!(
                        floor <= ceiling,
                        "target {:?} sets render_adaptive_min = {floor} above its \
                         video_quality of {ceiling}, which leaves the adaptive walk nowhere \
                         to go",
                        target.name
                    );
                }
            } else {
                anyhow::ensure!(
                    target.video_quality.is_none(),
                    "target {:?} sets video_quality, which is the dial of a VP9 stream, \
                     and this target sends none — its tiles' codec has its own, \
                     image_quality. Set render_motion to stream the cells in motion, or \
                     render_type = \"video\" to stream the whole desktop",
                    target.name
                );
                // The adaptive switch moves a stream's quality and nothing else. A
                // still is sent once: there is no next frame to sharpen a tile the
                // link coarsened, so tiles keep the quality they were configured
                // with, lossy or not — and turning a walk off where none runs says
                // as little as turning it on.
                anyhow::ensure!(
                    target.render_adaptive.is_none(),
                    "target {:?} sets render_adaptive, which moves the quality of a VP9 \
                     stream, and this target sends none — a tile is sent once, at the \
                     quality it was configured with. Set render_motion to stream the cells \
                     in motion, or render_type = \"video\" to stream the whole desktop",
                    target.name
                );
                anyhow::ensure!(
                    target.render_adaptive_min.is_none(),
                    "target {:?} sets render_adaptive_min, the floor of a VP9 stream's \
                     adaptive walk, and this target sends no stream to walk",
                    target.name
                );
            }
        }
        Ok(config)
    }

    /// Resolve the runtime configuration of a managed local instance: its private
    /// Unix socket and a freshly minted token.
    ///
    /// Both are arguments here rather than a default that
    /// `[server]` could override, which is what [`Audience::Embedded`] enforces on
    /// the way in. `[branding]` is the one thing such a config *may* say about the
    /// gateway itself: it names the instance, and multiple local instances are
    /// easier to tell apart if they can be called different things.
    ///
    /// `state_dir` is the instance directory, where `[meter]` keeps its database.
    #[cfg(all(feature = "embedded-gateway", unix))]
    pub fn resolve_embedded(
        self,
        token: EmbeddedToken,
        socket_path: PathBuf,
        state_dir: &Path,
    ) -> anyhow::Result<AppConfig> {
        let branding = Self::resolve_branding(self.branding.as_ref())?;
        Ok(AppConfig {
            // Only the native control plane reaches this listener. It owns the TCP
            // origin a browser addresses and proxies both HTTP and WebSockets here.
            listen: ListenAddr::Unix(socket_path),
            targets: self.targets,
            auth: GatewayAuth::Token(token),
            airplay: Self::resolve_airplay(self.airplay, &branding)?,
            branding,
            dev_hostname: None,
            meter: Self::resolve_meter(self.meter, state_dir),
        })
    }

    /// The `[meter]` table resolved, its database placed in `state_dir`. `None` unless
    /// the table says `enabled = true`. Its values were checked by [`Self::parse_with`].
    fn resolve_meter(section: Option<MeterSection>, state_dir: &Path) -> Option<MeterConfig> {
        section.filter(|section| section.enabled).map(|section| MeterConfig {
            // `join` keeps an absolute path as written.
            database: state_dir.join(section.database.as_deref().unwrap_or(Path::new(METER_DATABASE))),
            max_records: section.max_records,
        })
    }

    /// The `[airplay]` table resolved: the speaker is named after the gateway, so a
    /// Mac's Sound menu says which gateway it plays to, and marked as remotex's, so
    /// it says what the speaker is. Checked by [`Self::parse_with`].
    ///
    /// The mDNS instance is `<12 hex digits>@<name>`, one DNS label of at most 63
    /// bytes, and a longer one is registered and then never sent: a branding that
    /// long is refused rather than a speaker no Mac finds.
    fn resolve_airplay(section: Option<AirPlaySection>, branding: &Branding) -> anyhow::Result<Option<AirPlayConfig>> {
        const MAX_NAME_BYTES: usize = 63 - "000000000000@".len();
        section
            .map(|section| {
                let name = format!("{} - remotex", branding.text);
                anyhow::ensure!(
                    name.len() <= MAX_NAME_BYTES,
                    "[airplay] names the speaker {name:?}, {} bytes, and mDNS takes at most \
                     {MAX_NAME_BYTES}. Shorten [branding].text to {} bytes",
                    name.len(),
                    MAX_NAME_BYTES - " - remotex".len()
                );
                Ok(AirPlayConfig { name, password: section.password })
            })
            .transpose()
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
    ///
    /// For checking a config that may not be in any file: the state directory is the
    /// working directory, and nothing is opened in it.
    pub fn resolve(self) -> anyhow::Result<AppConfig> {
        self.resolve_with(None, Path::new(""))
    }

    /// Resolve the runtime configuration: validate the web-login credential and
    /// carry over every target profile (the browser picks one after login).
    ///
    /// `listen` is `--listen`/`REMOTEX_LISTEN` when either was given, and it wins
    /// over `[server].listen`. That is the whole precedence: one address, from the
    /// command line if it is there and from the file otherwise.
    ///
    /// `state_dir` is where `[meter]` keeps its database; see [`state_dir`].
    pub fn resolve_with(self, listen: Option<&str>, state_dir: &Path) -> anyhow::Result<AppConfig> {
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
        let branding = Self::resolve_branding(self.branding.as_ref())?;
        Ok(AppConfig {
            listen,
            // Non-empty is guaranteed by `parse`.
            targets: self.targets,
            auth: GatewayAuth::Login(site_passwd),
            airplay: Self::resolve_airplay(self.airplay, &branding)?,
            branding,
            dev_hostname: server
                .dev_subdomain
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(dev_hostname)
                .transpose()
                .context("invalid [server].dev_subdomain")?,
            meter: Self::resolve_meter(self.meter, state_dir),
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
    /// Where the gateway writes what it keeps between runs, outside the replaced files.
    state_dir: PathBuf,
}

/// The `[meter]` database's file name when the config names none.
const METER_DATABASE: &str = "meter.sqlite3";

/// The state directory of a gateway serving the config at `config`: the installation's
/// when that is the installed config, and otherwise the config file's own directory.
pub fn state_dir(config: &Path) -> PathBuf {
    if let Some(layout) = installed_layout()
        && layout.config == config
    {
        return layout.state_dir;
    }
    config.parent().map_or_else(PathBuf::new, Path::to_path_buf)
}

/// Resolve the package-manager layout or the container image's versioned layout
/// from the executable that is actually running.
fn installed_layout() -> Option<InstalledLayout> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    installed_layout_for_exe(&exe)
}

fn installed_layout_for_exe(exe: &Path) -> Option<InstalledLayout> {
    let bin_dir = exe.parent()?;

    // Native Linux packages own the executable at its FHS path. Configuration is
    // administrator-created under /etc, not under /usr: package removal must not
    // delete a file containing credentials.
    if bin_dir == Path::new("/usr/bin") {
        return Some(InstalledLayout {
            config: "/etc/remotex/remotex.toml".into(),
            state_dir: "/var/lib/remotex".into(),
        });
    }

    // The macOS package is the same direct layout under the locally managed
    // prefix. Its configuration follows that prefix as well.
    if bin_dir == Path::new("/usr/local/bin") {
        return Some(InstalledLayout {
            config: "/usr/local/etc/remotex/remotex.toml".into(),
            state_dir: "/usr/local/var/remotex".into(),
        });
    }

    // The Windows package installs the same tree under %ProgramFiles%\remotex:
    // <root>\bin\remotex.exe, and the tree is relocatable. Its configuration lives
    // outside that tree, under %ProgramData%, for the same reason as /etc above —
    // replacing the unpacked release must not touch a file holding credentials.
    #[cfg(windows)]
    if bin_dir.file_name().is_some_and(|name| name.eq_ignore_ascii_case("bin"))
        && let Some(program_data) = std::env::var_os("ProgramData")
    {
        let program_data = PathBuf::from(program_data).join("remotex");
        return Some(InstalledLayout {
            config: program_data.join("remotex.toml"),
            state_dir: program_data,
        });
    }

    // The container image (packaging/Dockerfile) puts the binary at
    // <prefix>/versions/<version>/bin/remotex, while configuration and state live
    // outside the version, under /opt/remotex/etc and /opt/remotex/var.
    let version_root = bin_dir.parent()?;
    let versions_dir = version_root.parent()?;
    if versions_dir.file_name()? != "versions" {
        return None;
    }
    let prefix = versions_dir.parent()?;
    Some(InstalledLayout {
        config: prefix.join("etc/remotex.toml"),
        state_dir: prefix.join("var"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_paths_follow_each_install_layout() {
        let linux = installed_layout_for_exe(Path::new("/usr/bin/remotex")).unwrap();
        assert_eq!(linux.config, Path::new("/etc/remotex/remotex.toml"));
        assert_eq!(linux.state_dir, Path::new("/var/lib/remotex"));

        let mac = installed_layout_for_exe(Path::new("/usr/local/bin/remotex")).unwrap();
        assert_eq!(mac.config, Path::new("/usr/local/etc/remotex/remotex.toml"));
        assert_eq!(mac.state_dir, Path::new("/usr/local/var/remotex"));

        // The container's tree is a Unix one; on Windows any `bin` directory is
        // the package's tree, which is the arm below.
        #[cfg(unix)]
        {
            let quick = installed_layout_for_exe(Path::new(
                "/srv/remotex/versions/0.0.144/bin/remotex",
            ))
            .unwrap();
            assert_eq!(quick.config, Path::new("/srv/remotex/etc/remotex.toml"));
            assert_eq!(quick.state_dir, Path::new("/srv/remotex/var"));
        }

        #[cfg(windows)]
        {
            let installed = installed_layout_for_exe(Path::new(
                r"C:\Program Files\remotex\bin\remotex.exe",
            ))
            .unwrap();
            let program_data = PathBuf::from(std::env::var_os("ProgramData").unwrap());
            assert_eq!(installed.config, program_data.join("remotex").join("remotex.toml"));
            assert_eq!(installed.state_dir, program_data.join("remotex"));
        }

        assert!(installed_layout_for_exe(Path::new("/checkout/target/debug/remotex")).is_none());
    }

    #[test]
    fn a_config_outside_an_installation_keeps_state_in_its_own_directory() {
        assert_eq!(state_dir(Path::new("/home/me/remotex/uat.toml")), Path::new("/home/me/remotex"));
        assert_eq!(state_dir(Path::new("uat.toml")), Path::new(""), "the working directory");
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
            username = "u"
            password = "p"
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
        assert_eq!((t.username.as_str(), t.password.as_str(), t.domain.as_deref()), ("u", "p", None));
        assert!(!t.resize, "dynamic resize is opt-in");
        assert!(t.egfx(), "the graphics pipeline is on unless turned off");
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
            username = "u"
            password = "p"
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
            file.clone().resolve_with(Some("0.0.0.0:8080"), Path::new("")).unwrap().listen.to_string(),
            "0.0.0.0:8080"
        );
        // Absent, the file still decides.
        assert_eq!(
            file.clone().resolve_with(None, Path::new("")).unwrap().listen.to_string(),
            "127.0.0.1:1"
        );
        // And a config with no address at all falls back to the default.
        assert_eq!(
            ConfigFile::parse(&minimal())
                .unwrap()
                .resolve_with(None, Path::new(""))
                .unwrap()
                .listen
                .to_string(),
            DEFAULT_LISTEN
        );

        let err = file.resolve_with(Some("0.0.0.0"), Path::new("")).unwrap_err();
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
            username = "u"
            password = "p"
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

    /// Nothing is recorded until `enabled = true`; an enabled table names its database
    /// and may leave the rest to the defaults.
    #[test]
    fn meter_is_recorded_in_the_state_directory_unless_a_database_is_named() {
        let state = Path::new("/var/lib/remotex");
        let meter = |table: &str| {
            let toml = format!("{table}\n{}", minimal());
            ConfigFile::parse(&toml).unwrap().resolve_with(None, state).unwrap().meter
        };
        assert_eq!(meter(""), None, "no [meter] records nothing");
        assert_eq!(
            meter("[meter]\nenabled = false\ndatabase = \"kept.sqlite3\""),
            None,
            "the settings are kept, the switch is off"
        );
        assert_eq!(
            meter("[meter]\nenabled = true"),
            Some(MeterConfig {
                database: PathBuf::from("/var/lib/remotex/meter.sqlite3"),
                max_records: 10_080,
            })
        );
        assert_eq!(
            meter("[meter]\nenabled = true\ndatabase = \"/srv/meter/remotex.sqlite3\"")
                .unwrap()
                .database,
            Path::new("/srv/meter/remotex.sqlite3")
        );
        assert_eq!(
            meter("[meter]\nenabled = true\ndatabase = \"meter/uat.sqlite3\"").unwrap().database,
            Path::new("/var/lib/remotex/meter/uat.sqlite3"),
            "a relative database is taken from the state directory"
        );

        assert_eq!(meter("[meter]\nenabled = true\nmax_records = 12").unwrap().max_records, 12);
        assert!(
            ConfigFile::parse(&format!(
                "[meter]\nenabled = true\ninterval_secs = 60\n{}",
                minimal()
            ))
            .is_err(),
            "the meter's second is not the file's to set"
        );
    }

    /// The keys are checked as written, enabled or not: a disabled table is still a
    /// config the operator means to turn on. And a table that never says which it is
    /// is refused rather than guessed at.
    #[test]
    fn a_meter_table_that_records_nothing_is_refused() {
        for (bad, says) in [
            ("database = \"u.sqlite3\"", "enabled"),
            ("enabled = false\ndatabase = \"\"", "[meter].database"),
            ("enabled = true\ndatabase = \"\"", "[meter].database"),
            ("enabled = true\ndatabase = \"u.sqlite3\"\ninterval_secs = 60", "interval_secs"),
            ("enabled = true\ndatabase = \"u.sqlite3\"\nmax_records = 0", "[meter].max_records"),
            ("enabled = true\ndatabase = \"u.sqlite3\"\nmax_count = 3", "max_count"),
        ] {
            let toml = format!("[meter]\n{bad}\n{}", minimal());
            let err = ConfigFile::parse(&toml).expect_err(bad);
            assert!(format!("{err:#}").contains(says), "{bad}: {err:#}");
        }
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
        // Every profile is carried over, in file order, for the picker.
        assert_eq!(config.targets.len(), 2);
        let win = &config.targets[0];
        assert_eq!(win.name, "win");
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
            username = "u"
            password = "p"
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
            username = "u"
            password = "p"
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
            username = "u"
            password = "p"
            host = "h1"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
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
            username = "u"
            password = "p"
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
            username = "u"
            password = "p"
            host = "h"
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_type, RenderType::Tiles);
        assert_eq!(t.render_subtype, None, "an unset base reads as png without being one");
        assert_eq!(t.render_subtype(), RenderSubtype::Png);
        assert_eq!(t.video_quality, None);
        assert_eq!(t.image_quality, None);
        assert_eq!(
            t.render_plan(Chroma::Subsampled),
            RenderPlan::Tiles { base: TileCodec::Png, motion: None, debug: false }
        );
    }

    /// The fixed lossy base: every tile through WebP at the one quality key.
    #[test]
    fn tiles_with_a_webp_base_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_type = "tiles"
            render_subtype = "webp"
            image_quality = 60
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_subtype(), RenderSubtype::Webp);
        assert_eq!(
            t.render_plan(Chroma::Subsampled),
            RenderPlan::Tiles {
                base: TileCodec::Webp { quality: 60 },
                motion: None,
                debug: false,
            }
        );
    }

    #[test]
    fn webp_without_a_quality_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_subtype = "webp"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("image_quality"), "{err:#}");
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
            username = "u"
            password = "p"
            host = "h"
            render_subtype = "webp"
            image_quality = 60
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan::Tiles {
                base: TileCodec::Webp { quality: 60 },
                motion: None,
                debug: false,
            }
        );
    }

    #[test]
    fn tiles_with_a_classify_base_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_type = "tiles"
            render_subtype = "classify"
            image_quality = 60
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_subtype(), RenderSubtype::Classify);
        assert_eq!(
            t.render_plan(Chroma::Subsampled),
            RenderPlan::Tiles {
                base: TileCodec::Classify {
                    quality: 60,
                    debug: false,
                },
                motion: None,
                debug: false,
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
            username = "u"
            password = "p"
            host = "h"
            render_subtype = "classify"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("image_quality"), "{err:#}");
    }

    /// The classifier as a motion base: a settled cell is classified
    /// (photographic WebP, text lossless) while the moving regions become video.
    #[test]
    fn motion_streams_over_a_classify_base() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_motion = true
            render_subtype = "classify"
            image_quality = 60
            video_quality = 30
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan::Tiles {
                base: TileCodec::Classify {
                    quality: 60,
                    debug: false,
                },
                motion: Some(MotionEncode { quality: 30, adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN), chroma: Chroma::Subsampled }),
                debug: false,
            }
        );
    }

    /// The chroma key reaches both kinds of stream, and a target that writes none
    /// gets the browser's answer: unset resolves exactly as `"auto"` does. A target
    /// that names a profile is not moved by the decoder in front of it — that is the
    /// whole difference between selecting a chroma and leaving it to be resolved.
    #[test]
    fn render_chroma_reaches_the_stream_and_defaults_to_auto() {
        let video = |extra: &str, decoder| {
            ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                render_type = "video"
                video_quality = 100
                {extra}
                "#
            ))
            .unwrap()
            .targets[0]
                .render_plan(decoder)
        };
        let stream = |chroma| RenderPlan::Video {
            quality: 100,
            adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
            chroma,
        };
        // Unset is `auto`, key for key.
        assert_eq!(video("", Chroma::Full), stream(Chroma::Full));
        assert_eq!(video("", Chroma::Subsampled), stream(Chroma::Subsampled));
        assert_eq!(video("render_chroma = \"auto\"", Chroma::Full), video("", Chroma::Full));
        // A named profile asks nobody.
        assert_eq!(video("render_chroma = \"420\"", Chroma::Full), stream(Chroma::Subsampled));
        assert_eq!(video("render_chroma = \"444\"", Chroma::Subsampled), stream(Chroma::Full));

        // And the motion shape reads the same key the same way.
        let motion = |extra: &str, decoder| {
            ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                render_motion = true
                video_quality = 30
                {extra}
                "#
            ))
            .unwrap()
            .targets[0]
                .render_plan(decoder)
        };
        let moving = |chroma| RenderPlan::Tiles {
            base: TileCodec::Png,
            motion: Some(MotionEncode {
                quality: 30,
                adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                chroma,
            }),
            debug: false,
        };
        assert_eq!(motion("", Chroma::Full), moving(Chroma::Full));
        assert_eq!(motion("render_chroma = \"420\"", Chroma::Full), moving(Chroma::Subsampled));
        assert_eq!(motion("render_chroma = \"444\"", Chroma::Subsampled), moving(Chroma::Full));
    }

    /// A chroma for a target that streams nothing is refused, like a codec for
    /// audio that was never turned on; and the key takes only the two samplings
    /// VP9 profiles 0 and 1 are.
    #[test]
    fn render_chroma_without_a_stream_is_refused() {
        for keys in [
            "",
            "render_subtype = \"webp\"\nimage_quality = 60",
        ] {
            let err = ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
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
            username = "u"
            password = "p"
            host = "h"
            render_type = "video"
            video_quality = 100
            render_chroma = "422"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("422"), "{err:#}");
    }

    /// `auto` is the third answer, and the only one that reads the browser: the
    /// same target resolves to 4:4:4 for a decoder that takes profile 1 and to
    /// 4:2:0 for one that does not, on both kinds of stream. This is what a target
    /// watched from a desktop and an iPad is written as, once.
    #[test]
    fn auto_chroma_follows_the_browser_on_both_kinds_of_stream() {
        let target = |render: &str| {
            ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                {render}
                render_chroma = "auto"
                "#
            ))
            .unwrap()
            .targets
            .remove(0)
        };

        let video = target("render_type = \"video\"\nvideo_quality = 100");
        assert_eq!(video.render_chroma, Some(ChromaChoice::Auto));
        assert_eq!(
            video.render_plan(Chroma::Full),
            RenderPlan::Video { quality: 100, adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN), chroma: Chroma::Full }
        );
        assert_eq!(
            video.render_plan(Chroma::Subsampled),
            RenderPlan::Video { quality: 100, adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN), chroma: Chroma::Subsampled }
        );

        let motion = target("render_motion = true\nvideo_quality = 30");
        let plan = |decoder| match motion.render_plan(decoder) {
            RenderPlan::Tiles { motion: Some(MotionEncode { chroma, .. }), .. } => chroma,
            other => panic!("a motion target must resolve to a motion plan: {other:?}"),
        };
        assert_eq!(plan(Chroma::Full), Chroma::Full);
        assert_eq!(plan(Chroma::Subsampled), Chroma::Subsampled);

        // And it is refused where the other two are, for the same reason: there is
        // no stream for the browser's answer to resolve.
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_chroma = "auto"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_chroma"), "{err:#}");
    }

    /// The TUI reads a config file with no browser in front of it, so `auto` — the
    /// default, and therefore most cards — is the one target it cannot describe by
    /// resolving. It says the chroma is the browser's to pick; a target that
    /// selected one names the sampling it selected.
    ///
    /// The two readings have to stay apart on the card, because on the wire they are
    /// different decisions: `4:2:0` here is every browser held to the subsampled
    /// stream, where `chroma auto` is the profile-1 decoders getting the colour.
    #[test]
    fn a_target_card_says_whether_the_chroma_is_auto_or_selected() {
        let summary = |extra: &str| {
            ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                render_type = "video"
                video_quality = 60
                {extra}
                "#
            ))
            .unwrap()
            .targets[0]
                .render_summary()
        };
        assert_eq!(summary(""), "video q60 chroma auto · adaptive ≥20");
        assert_eq!(summary("render_chroma = \"auto\""), summary(""));
        assert_eq!(summary("render_chroma = \"420\""), "video q60 4:2:0 · adaptive ≥20");
        assert_eq!(summary("render_chroma = \"444\""), "video q60 4:4:4 · adaptive ≥20");
        // With the walk off the chroma is the last thing on the line, and still the
        // stream's own segment rather than a clause hung off whatever precedes it.
        assert_eq!(summary("render_adaptive = false"), "video q60 chroma auto");
        // A motion target reads the same, on the encode the chroma belongs to.
        assert_eq!(
            parse_target("render_motion = true\nvideo_quality = 40")
                .unwrap()
                .targets[0]
                .render_summary(),
            "motion · base lossless png, moving stream q40 chroma auto · adaptive ≥20"
        );

        // A target with no stream has no chroma to state, auto or otherwise.
        let tiles = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            "#,
        )
        .unwrap();
        assert_eq!(tiles.targets[0].render_summary(), "tiles · lossless png");
    }

    /// The grid is the debug aid no encoder can see: it belongs to the two
    /// transport that cuts damage into tiles, and it leaves the render plan alone.
    #[test]
    fn the_tile_grid_overlay_is_opt_in_and_refused_by_video() {
        for render in ["render_type = \"tiles\"", "render_motion = true\nvideo_quality = 30"] {
            let cfg = ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                {render}
                render_grid_debug = true
                "#
            ))
            .unwrap();
            let target = &cfg.targets[0];
            assert!(target.render_grid_debug, "{render}");
            // The lattice costs the encoders nothing, so the plan they read is the
            // plan they would have read without it.
            let mut plain = target.clone();
            plain.render_grid_debug = false;
            assert_eq!(target.render_plan(Chroma::Subsampled), plain.render_plan(Chroma::Subsampled), "{render}");
        }

        // Off unless asked for, and then there is no lattice to state either.
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            "#,
        )
        .unwrap();
        assert!(!cfg.targets[0].render_grid_debug);

        // `video` sends no tiles, so it has no boundary to draw.
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_type = "video"
            video_quality = 60
            render_grid_debug = true
            "#,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("render_grid_debug"), "{message}");
        assert!(message.contains("sends no tiles"), "{message}");
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
            username = "u"
            password = "p"
            host = "h"
            render_subtype = "classify"
            image_quality = 60
            render_classify_debug = true
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan::Tiles {
                base: TileCodec::Classify {
                    quality: 60,
                    debug: true,
                },
                motion: None,
                debug: false,
            }
        );

        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_subtype = "webp"
            image_quality = 60
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
            username = "u"
            password = "p"
            host = "h"
            render_subtype = "webp"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("image_quality"), "{err:#}");
    }

    #[test]
    fn an_image_quality_out_of_range_is_rejected() {
        for q in ["0", "101"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                render_subtype = "webp"
                image_quality = {q}
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
            username = "u"
            password = "p"
            host = "h"
            render_type = "video"
            video_quality = 60
            "#,
        )
        .expect("video with a quality");
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan::Video {
                quality: 60,
                adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                chroma: Chroma::Subsampled
            }
        );
    }

    #[test]
    fn video_without_a_quality_streams_at_the_default() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_type = "video"
            "#,
        )
        .expect("video needs no quality");
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan::Video {
                quality: DEFAULT_VIDEO_QUALITY,
                adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                chroma: Chroma::Subsampled
            }
        );
    }

    #[test]
    fn a_video_quality_out_of_range_is_rejected() {
        for q in ["0", "101"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                render_type = "video"
                video_quality = {q}
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
        for subtype in ["png", "webp", "classify"] {
            let err = ConfigFile::parse(&format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                render_type = "video"
                render_subtype = "{subtype}"
                video_quality = 60
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
    /// "png"` under `tiles`, with or without `render_motion`, is exactly what
    /// leaving it out is, and
    /// the refusal above is about `video` having no base tiles, not about the
    /// value.
    #[test]
    fn an_explicit_png_base_is_the_default_under_tiles_and_motion() {
        for keys in [
            "render_type = \"tiles\"",
            "render_motion = true\nvideo_quality = 30",
        ] {
            let explicit = format!(
                "[[targets]]\nname = \"a\"\nprotocol = \"rdp\"\nhost = \"h\"\nusername = \"u\"\npassword = \"p\"\n{keys}\n\
                 render_subtype = \"png\"\n"
            );
            let implicit = format!(
                "[[targets]]\nname = \"a\"\nprotocol = \"rdp\"\nhost = \"h\"\nusername = \"u\"\npassword = \"p\"\n{keys}\n"
            );
            let explicit = ConfigFile::parse(&explicit).unwrap_or_else(|e| panic!("{keys}: {e:#}"));
            let implicit = ConfigFile::parse(&implicit).unwrap_or_else(|e| panic!("{keys}: {e:#}"));
            assert_eq!(explicit.targets[0].render_subtype, Some(RenderSubtype::Png), "{keys}");
            assert_eq!(implicit.targets[0].render_subtype, None, "{keys}");
            assert_eq!(
                explicit.targets[0].render_plan(Chroma::Subsampled),
                implicit.targets[0].render_plan(Chroma::Subsampled),
                "{keys}: naming the default changes nothing"
            );
        }
    }

    /// The motion keys belong to `render_motion`, and `video` is not a second place
    /// to put them — its stream has no cells to find in motion. Both of them are
    /// asserted here because `video` is the newest transport and the one most likely
    /// to be tried with them. The quality is deliberately absent: since the two
    /// stream dials merged it is `video_quality`, which `video` reads for its own
    /// stream, and `video_quality_is_refused_where_nothing_streams` owns its rule.
    #[test]
    fn video_refuses_the_motion_keys() {
        let err = parse_target("render_type = \"video\"\nvideo_quality = 60\nrender_motion = true")
            .unwrap_err();
        assert!(format!("{err:#}").contains("render_motion with render_type"), "{err:#}");
        let err =
            parse_target("render_type = \"video\"\nvideo_quality = 60\nrender_motion_debug = true")
                .unwrap_err();
        assert!(format!("{err:#}").contains("without render_motion"), "{err:#}");
    }

    #[test]
    fn image_quality_on_lossless_png_is_rejected() {
        // render_type/subtype default to tiles/png, so a stray quality has
        // nothing to apply to — with or without the defaults written out.
        for keys in ["", "render_type = \"tiles\"\nrender_subtype = \"png\"\n"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                {keys}image_quality = 50
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
            username = "u"
            password = "p"
            host = "h"
            render_type = "adaptive"
            "#,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("tiles") && msg.contains("video"), "{msg}");
    }

    // ---- the motion switch ----

    /// The configuration the fixed dial cannot express at all, and the one the
    /// whole scheme is for: text and flat UI stay perfect and lossless, and only
    /// what moves goes to a stream.
    #[test]
    fn motion_over_a_lossless_base_is_accepted() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_motion = true
            video_quality = 10
            "#,
        )
        .unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.render_type, RenderType::Tiles);
        assert!(t.render_motion);
        assert_eq!(t.render_subtype(), RenderSubtype::Png);
        assert_eq!(
            t.render_plan(Chroma::Subsampled),
            RenderPlan::Tiles {
                base: TileCodec::Png,
                motion: Some(MotionEncode { quality: 10, adaptive: Some(10), chroma: Chroma::Subsampled }),
                debug: false,
            }
        );
    }

    /// A lossy base keeps its own meaning — `render_subtype` and `image_quality` are
    /// what a settled cell gets — while the moving regions stream at their own
    /// quality. What a settled cell gets is a still picture; what is moving is not
    /// one at all, which is why the two qualities are separate keys.
    #[test]
    fn a_lossy_base_and_the_motion_stream_keep_their_own_qualities() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_motion = true
            render_subtype = "webp"
            image_quality = 60
            video_quality = 10
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan::Tiles {
                base: TileCodec::Webp { quality: 60 },
                motion: Some(MotionEncode { quality: 10, adaptive: Some(10), chroma: Chroma::Subsampled }),
                debug: false,
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
    ///
    /// Resolved against a browser that takes only 4:2:0, which is what keeps the
    /// default chroma and a selected `"444"` two cases rather than one: `auto` in
    /// front of a profile-1 decoder *is* the 4:4:4 plan, down to the card.
    #[test]
    fn every_render_combination_describes_itself() {
        let cases = [
            ("tiles over lossless png, the default", "render_type = \"tiles\"", "tiles · lossless png"),
            (
                "tiles over fixed-quality webp",
                "render_subtype = \"webp\"\nimage_quality = 60",
                "tiles · webp q60",
            ),
            (
                "tiles behind the classifier",
                "render_subtype = \"classify\"\nimage_quality = 60",
                "tiles · classified png / webp q60",
            ),
            (
                "the classifier's debug outlines, a different session to be looking at",
                "render_subtype = \"classify\"\nimage_quality = 60\nrender_classify_debug = true",
                "tiles · classified png / webp q60 (debug outlines)",
            ),
            (
                "motion over a classify base",
                "render_motion = true\nrender_subtype = \"classify\"\nimage_quality = 60\nvideo_quality = 15",
                "motion · base classified png / webp q60, moving stream q15 4:2:0 · adaptive ≥15",
            ),
            (
                "motion over a lossless base",
                "render_motion = true\nvideo_quality = 30",
                "motion · base lossless png, moving stream q30 4:2:0 · adaptive ≥20",
            ),
            (
                "motion over a lossy base",
                "render_motion = true\nrender_subtype = \"webp\"\nimage_quality = 70\nvideo_quality = 40",
                "motion · base webp q70, moving stream q40 4:2:0 · adaptive ≥20",
            ),
            (
                "the debug outlines, which are a different session to be looking at",
                "render_motion = true\nvideo_quality = 30\nrender_motion_debug = true",
                "motion · base lossless png, moving stream q30 4:2:0 · adaptive ≥20 (debug outlines)",
            ),
            (
                "the whole desktop as one stream",
                "render_type = \"video\"\nvideo_quality = 60",
                "video q60 4:2:0 · adaptive ≥20",
            ),
            (
                "the whole desktop as one stream with every pixel's colour",
                "render_type = \"video\"\nvideo_quality = 60\nrender_chroma = \"444\"",
                "video q60 4:4:4 · adaptive ≥20",
            ),
            (
                "a stream per region with every pixel's colour",
                "render_motion = true\nvideo_quality = 40\nrender_chroma = \"444\"",
                "motion · base lossless png, moving stream q40 4:4:4 · adaptive ≥20",
            ),
            (
                "the whole desktop as one stream, held to its dial",
                "render_type = \"video\"\nvideo_quality = 60\nrender_adaptive = false",
                "video q60 4:2:0",
            ),
            (
                "a stream per region, held to its dial",
                "render_motion = true\nvideo_quality = 30\nrender_adaptive = false",
                "motion · base lossless png, moving stream q30 4:2:0",
            ),
        ];

        let mut seen: Vec<String> = Vec::new();
        for (what, keys, expected) in cases {
            let toml = format!(
                "[server]\n{}\n\n[[targets]]\nname = \"t\"\nprotocol = \"rdp\"\n\
                 host = \"192.0.2.10\"\nusername = \"u\"\npassword = \"p\"\n{keys}\n",
                site_passwd_line()
            );
            let cfg = ConfigFile::parse(&toml)
                .unwrap_or_else(|e| panic!("{what} should be a legal dial: {e:#}"));
            let described = cfg.targets[0].render_plan(Chroma::Subsampled).describe();
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

    /// The moving encode is the whole point of the switch, and the switch alone is
    /// enough: the dial it reads defaults like the walk that moves it.
    #[test]
    fn motion_without_a_motion_quality_takes_the_default() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_motion = true
            "#,
        )
        .expect("motion needs no quality");
        assert_eq!(
            motion_of(cfg.targets[0].render_plan(Chroma::Subsampled)),
            Some(MotionEncode {
                quality: DEFAULT_VIDEO_QUALITY,
                adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                chroma: Chroma::Subsampled
            })
        );
    }

    #[test]
    fn a_motion_quality_out_of_range_is_rejected() {
        for q in ["0", "101"] {
            let toml = format!(
                r#"
                [[targets]]
                name = "a"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "h"
                render_motion = true
                video_quality = {q}
                "#
            );
            let err = ConfigFile::parse(&toml).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("video_quality"), "q={q}: {msg}");
            assert!(msg.contains("1–100"), "q={q}: {msg}");
        }
    }

    /// A lossy base under `render_motion` still needs its own quality: the subtype names
    /// what a *settled* cell is encoded as, and that is not the motion quality.
    #[test]
    fn a_lossy_motion_base_still_needs_its_own_quality() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_motion = true
            render_subtype = "webp"
            video_quality = 10
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("image_quality"), "{err:#}");
    }

    /// The QA overlay rides on the motion path and is off unless asked for, so
    /// a target that never turns it on cannot be paying for it by accident.
    #[test]
    fn the_motion_debug_overlay_is_opt_in_and_belongs_to_motion() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_motion = true
            video_quality = 10
            render_motion_debug = true
            "#,
        )
        .unwrap();
        assert!(matches!(cfg.targets[0].render_plan(Chroma::Subsampled), RenderPlan::Tiles { debug: true, .. }));

        let plain = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            "#,
        )
        .unwrap();
        assert!(
            matches!(plain.targets[0].render_plan(Chroma::Subsampled), RenderPlan::Tiles { debug: false, .. }),
            "the overlay defaulted on"
        );

        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_motion_debug = true
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_motion_debug"), "{err:#}");
    }

    /// A lossless base takes no quality, and `render_motion` does not change that:
    /// the discount has its own dial and the base still has none.
    #[test]
    fn image_quality_on_a_lossless_motion_base_is_rejected() {
        let err = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_motion = true
            image_quality = 60
            video_quality = 10
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("lossless \"png\" base"), "{err:#}");
    }

    /// `video_quality` is a stream's dial and nothing else's, so a target
    /// that sends no stream has nothing for it to describe — most likely a config
    /// that lost its `render_motion` or its `render_type`. The tiles' own quality
    /// is `image_quality`, and the error says so.
    #[test]
    fn video_quality_is_refused_where_nothing_streams() {
        for keys in [
            "video_quality = 60",
            "render_type = \"tiles\"\nvideo_quality = 60",
            "render_subtype = \"webp\"\nimage_quality = 70\nvideo_quality = 60",
        ] {
            let err = parse_target(keys).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("sends none"), "{keys}: {msg}");
            assert!(msg.contains("image_quality"), "{keys}: {msg}");
        }
    }

    /// The other half of one key serving both: each way of carrying a stream reads
    /// it under the same name, takes the same default, and refuses the same range.
    #[test]
    fn both_ways_of_streaming_read_the_same_quality_key() {
        for keys in ["render_type = \"video\"", "render_motion = true"] {
            let err = parse_target(&format!("{keys}\nvideo_quality = 0")).unwrap_err();
            assert!(format!("{err:#}").contains("out of range"), "{keys}");
        }
        // And both resolve it into the plan, at the same name.
        assert_eq!(
            parse_target("render_type = \"video\"\nvideo_quality = 60")
                .unwrap()
                .targets[0]
                .render_plan(Chroma::Subsampled),
            RenderPlan::Video {
                quality: 60,
                adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                chroma: Chroma::Subsampled
            }
        );
        assert_eq!(
            motion_of(
                parse_target("render_motion = true\nvideo_quality = 60")
                    .unwrap()
                    .targets[0]
                    .render_plan(Chroma::Subsampled)
            ),
            Some(MotionEncode {
                quality: 60,
                adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                chroma: Chroma::Subsampled
            })
        );
    }

    /// And the other way round: `video` has no base tiles, so no base quality.
    #[test]
    fn image_quality_is_refused_under_video() {
        let err =
            parse_target("render_type = \"video\"\nvideo_quality = 60\nimage_quality = 60")
                .unwrap_err();
        assert!(format!("{err:#}").contains("sends no tiles"), "{err:#}");
    }

    /// More likely a `render_motion` that was never written than a deliberate
    /// choice, so it is worth saying so rather than ignoring the key. The quality
    /// is not tested here: it is no longer motion's own, and
    /// `video_quality_is_refused_where_nothing_streams` is its rule.
    #[test]
    fn the_motion_overlay_is_refused_without_the_switch() {
        let err =
            parse_target("render_subtype = \"webp\"\nimage_quality = 60\nrender_motion_debug = true")
                .unwrap_err();
        assert!(format!("{err:#}").contains("without render_motion"), "{err:#}");
    }

    /// Motion lives in the shared sink, independent of which engine produced the
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
            render_motion = true
            video_quality = 10
            "#,
        )
        .expect("motion is independent of the VNC subtype");
        assert_eq!(
            motion_of(cfg.targets[0].render_plan(Chroma::Subsampled)),
            Some(MotionEncode { quality: 10, adaptive: Some(10), chroma: Chroma::Subsampled })
        );
    }

    /// The switch that keeps the whole motion path off: nothing but `render_motion`
    /// resolves a moving encode, so every configuration that shipped before it
    /// existed still encodes every tile the one way.
    #[test]
    fn nothing_but_the_switch_resolves_a_motion_encode() {
        let cfg = ConfigFile::parse(
            r#"
            [[targets]]
            name = "a"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"

            [[targets]]
            name = "b"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "h"
            render_type = "tiles"
            render_subtype = "webp"
            image_quality = 60
            "#,
        )
        .unwrap();
        for t in &cfg.targets {
            assert_eq!(motion_of(t.render_plan(Chroma::Subsampled)), None, "target {:?}", t.name);
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
            username = "u"
            password = "p"
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
            "{pin}render_type = \"video\"\nvideo_quality = 60"
        )))
        .expect_err("a 5K pin on a video stream parsed");
        assert!(format!("{err:#}").contains("3840"), "{err:#}");
        assert!(format!("{err:#}").contains("tiles"), "{err:#}");
        ConfigFile::parse(&rdp_toml(&format!(
            "{pin}render_motion = true\nrender_subtype = \"webp\"\nimage_quality = 60\n\
             video_quality = 60"
        )))
        .expect_err("a 5K pin on a region stream parsed");
        ConfigFile::parse(&rdp_toml(&format!("{pin}render_type = \"tiles\"")))
            .expect("a 5K pin on a tiles target is an oversized desktop that scrolls");
        for pin in ["width = 3840\nheight = 2400", "width = 2400\nheight = 3840"] {
            ConfigFile::parse(&rdp_toml(&format!("{pin}\nrender_type = \"video\"\nvideo_quality = 60")))
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
            username = "u"
            password = "p"
            host = "10.0.0.5"
            vnc_password = "hunter2"
            "#,
            site_passwd_line()
        ))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("vnc_password") && msg.contains("vnc"), "{msg}");
    }

    /// The RDP client logs on only through NLA, so a target without both halves of
    /// its credential could never connect and is refused up front.
    #[test]
    fn an_rdp_target_needs_username_and_password() {
        for keys in ["", "username = \"u\"", "password = \"p\""] {
            let err = ConfigFile::parse(&format!(
                "[server]\n{}\n[[targets]]\nname = \"w\"\nprotocol = \"rdp\"\nhost = \"h\"\n{keys}\n",
                site_passwd_line()
            ))
            .unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("username") && msg.contains("password"), "{keys}: {msg}");
        }
    }

    /// The clipboard is every engine's: generic VNC's Extended Clipboard, Apple's
    /// pasteboard, and MS-RDPECLIP on the RDP client's own channel.
    #[test]
    fn clipboard_is_taken_by_every_protocol() {
        for (protocol, host) in [("vnc", "10.0.0.4"), ("rdp", "10.0.0.5")] {
            let config = ConfigFile::parse(&format!(
                r#"
                [server]
                {}

                [[targets]]
                name = "box"
                protocol = "{protocol}"
                host = "{host}"
                username = "u"
                password = "p"
                clipboard = true
                "#,
                site_passwd_line()
            ))
            .unwrap()
            .resolve()
            .unwrap();
            assert!(config.targets[0].clipboard, "{protocol}");
        }
    }

    /// RDP and generic VNC both take audio; Apple's standard Screen Sharing is
    /// the one target that refuses it by name.
    ///
    /// The error has to say what does carry it, because the mistake behind the
    /// key is a belief about what the subtype does rather than a typo — and a
    /// target that silently ignored it would be a desktop that is simply quiet,
    /// with nothing anywhere to say why.
    #[test]
    fn audio_belongs_to_rdp_and_generic_vnc() {
        // A plain `vnc` target asks a generic server for wlshare's audio
        // extension, and gets silence from one that does not speak it. That is
        // discovery, not a config error.
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "wlshare"
            protocol = "vnc"
            host = "10.0.0.5"
            audio = true
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].audio);
        assert_eq!(
            config.targets[0].audio_source_format(),
            crate::vnc_audio::SOURCE_FORMAT
        );

        // An rdp target negotiates MS-RDPEA when it connects, and what the host
        // redirects is CD-quality PCM, which is the source format the encoder is
        // built from.
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "10.0.0.5"
            audio = true
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].audio);
        assert_eq!(config.targets[0].audio_source_format(), crate::audio::PCM_CD_QUALITY);
    }

    /// The camera rides MS-RDPECAM on RDP and wlshare's camera extension on a
    /// generic VNC target, so the key is accepted on both — opt-in (default off) on
    /// each — and refused on both Apple subtypes, whose Screen Sharing speaks no
    /// such extension.
    #[test]
    fn camera_rides_rdp_and_generic_vnc_and_is_refused_on_a_mac() {
        for subtype in ["ard", "ard-high-performance"] {
            let err = ConfigFile::parse(&format!(
                r#"
                [server]
                {}

                [[targets]]
                name = "mac"
                protocol = "vnc"
                subtype = "{subtype}"
                host = "10.0.0.5"
                username = "andrew"
                password = "h"
                camera = true
                "#,
                site_passwd_line()
            ))
            .and_then(|file| file.resolve())
            .unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains(&format!("camera on an {subtype} target")), "{rendered}");
            assert!(
                rendered.contains("wlshare's camera extension"),
                "the path a vnc target does have is named: {rendered}"
            );
        }

        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "10.0.0.5"
            camera = true

            [[targets]]
            name = "desk"
            protocol = "vnc"
            host = "10.0.0.7"
            camera = true

            [[targets]]
            name = "quiet"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "10.0.0.6"
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(config.targets[0].camera);
        assert!(config.targets[1].camera, "a generic vnc target asks wlshare for it");
        assert!(!config.targets[2].camera, "the camera is opt-in");
    }

    /// The microphone rides MS-RDPEAI on RDP and wlshare's microphone extension on a
    /// generic VNC target, and is refused on both Apple subtypes. On either it stands on
    /// its own: a remote records with or without redirected sound.
    #[test]
    fn microphone_rides_rdp_and_generic_vnc_and_is_refused_on_a_mac() {
        let parse = |target: &str| {
            ConfigFile::parse(&format!("[server]\n{}\n\n[[targets]]\n{target}", site_passwd_line()))
                .and_then(|file| file.resolve())
        };
        for subtype in ["ard", "ard-high-performance"] {
            let mac = parse(&format!(
                "name = \"mac\"\nprotocol = \"vnc\"\nsubtype = \"{subtype}\"\nhost = \"10.0.0.5\"\nusername = \"andrew\"\npassword = \"h\"\nmicrophone = true"
            ))
            .unwrap_err();
            let rendered = format!("{mac:#}");
            assert!(rendered.contains(&format!("microphone on an {subtype} target")), "{rendered}");
            assert!(rendered.contains("wlshare's microphone extension"), "{rendered}");
        }
        let vnc = parse("name = \"desk\"\nprotocol = \"vnc\"\nhost = \"10.0.0.7\"\nmicrophone = true").unwrap();
        assert!(vnc.targets[0].microphone, "a generic vnc target asks wlshare for it");
        let config = parse(
            "name = \"win\"\nprotocol = \"rdp\"\nusername = \"u\"\npassword = \"p\"\nhost = \"10.0.0.5\"\nmicrophone = true",
        )
        .unwrap();
        assert!(config.targets[0].microphone);
        assert!(!config.targets[0].audio, "the microphone does not need the remote's sound");
    }

    /// EGFX is RDP's, and refused on VNC by name — either value, since a key
    /// that could not do anything is a config error, not a preference. On RDP the
    /// key is read, and `false` is the bitmap path.
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

        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "10.0.0.5"
            egfx = false
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert!(!config.targets[0].egfx(), "the bitmap path is one key away");
    }

    /// An RDP resize is a graphics reset, so the bitmap path has none to offer and
    /// the pair is refused by name rather than left inert.
    #[test]
    fn resize_is_refused_on_rdp_without_the_graphics_pipeline() {
        let err = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "win"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "10.0.0.5"
            resize = true
            egfx = false
            "#,
            site_passwd_line()
        ))
        .and_then(ConfigFile::resolve)
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("resize"), "{rendered}");
        assert!(rendered.contains("egfx = false"), "{rendered}");
    }

    /// A Mac's sound arrives over AirPlay on either subtype, so the gateway-wide
    /// `[airplay]` table, not the target, is what decides it: every Mac carries
    /// audio exactly when the table is set, and the resolved config carries the
    /// speaker under the gateway's name.
    #[cfg(feature = "airplay")]
    #[test]
    fn a_macs_audio_follows_the_airplay_table() {
        for subtype in ["ard", "ard-high-performance"] {
            let target = format!(
                "[[targets]]\nname = \"mac\"\nprotocol = \"vnc\"\nsubtype = \"{subtype}\"\n\
                 host = \"10.0.0.5\"\nusername = \"andrew\"\npassword = \"h\"\n"
            );
            let silent = ConfigFile::parse(&format!("[server]\n{}\n{target}", site_passwd_line()))
                .unwrap()
                .resolve()
                .unwrap();
            assert!(!silent.targets[0].audio, "no [airplay], no sound");
            assert_eq!(silent.airplay, None);

            let config = ConfigFile::parse(&format!(
                "[server]\n{}\n[branding]\ntext = \"Studio\"\n[airplay]\npassword = \"sesame\"\n{target}",
                site_passwd_line()
            ))
            .unwrap()
            .resolve()
            .unwrap();
            assert!(config.targets[0].audio, "[airplay] carries every Mac's sound");
            assert_eq!(
                config.airplay,
                Some(AirPlayConfig { name: "Studio - remotex".into(), password: "sesame".into() })
            );
            assert_eq!(config.targets[0].audio_source_format(), crate::audio::PCM_CD_QUALITY);

            for key in ["audio = true", "audio = false"] {
                let err = ConfigFile::parse(&format!(
                    "[server]\n{}\n[airplay]\npassword = \"sesame\"\n{target}{key}\n",
                    site_passwd_line()
                ))
                .unwrap_err();
                let rendered = format!("{err:#}");
                assert!(rendered.contains(&format!("sets audio on an {subtype} target")), "{rendered}");
                assert!(rendered.contains("Remove the key"), "{rendered}");
            }

            let empty = ConfigFile::parse(&format!(
                "[server]\n{}\n[airplay]\npassword = \" \"\n{target}",
                site_passwd_line()
            ))
            .unwrap_err();
            assert!(format!("{empty:#}").contains("[airplay].password is empty"), "{empty:#}");
        }
    }

    /// A speaker no Mac could play through is refused rather than started.
    #[cfg(feature = "airplay")]
    #[test]
    fn airplay_without_a_mac_is_refused() {
        let vnc = "[[targets]]\nname = \"box\"\nprotocol = \"vnc\"\nhost = \"h\"\naudio = true\n";
        let err = ConfigFile::parse(&format!(
            "[server]\n{}\n[airplay]\npassword = \"sesame\"\n{vnc}",
            site_passwd_line()
        ))
        .unwrap_err();
        assert!(format!("{err:#}").contains("nothing would play through the speaker"), "{err:#}");
    }

    /// The speaker's name is one DNS label with the address in front of it, so a
    /// branding that would overflow it is refused rather than never announced.
    #[cfg(feature = "airplay")]
    #[test]
    fn an_airplay_name_past_a_dns_label_is_refused() {
        let config = |text: &str| {
            ConfigFile::parse(&format!(
                "[server]\n{}\n[branding]\ntext = \"{text}\"\n[airplay]\npassword = \"sesame\"\n\
                 [[targets]]\nname = \"mac\"\nprotocol = \"vnc\"\nsubtype = \"ard\"\nhost = \"h\"\n\
                 username = \"u\"\npassword = \"p\"\n",
                site_passwd_line()
            ))
            .unwrap()
            .resolve()
        };
        assert_eq!(config(&"a".repeat(40)).unwrap().airplay.unwrap().name.len(), 50);
        let err = config(&"a".repeat(41)).unwrap_err();
        assert!(format!("{err:#}").contains("Shorten [branding].text to 40 bytes"), "{err:#}");
    }

    /// A build without the speaker says so when asked for one, and its Macs
    /// carry no sound.
    #[cfg(not(feature = "airplay"))]
    #[test]
    fn a_build_without_airplay_refuses_the_table() {
        let mac = "[[targets]]\nname = \"mac\"\nprotocol = \"vnc\"\nsubtype = \"ard\"\nhost = \"h\"\n\
                   username = \"u\"\npassword = \"p\"\n";
        let err = ConfigFile::parse(&format!(
            "[server]\n{}\n[airplay]\npassword = \"sesame\"\n{mac}",
            site_passwd_line()
        ))
        .unwrap_err();
        assert!(format!("{err:#}").contains("without the airplay feature"), "{err:#}");
        let config = ConfigFile::parse(&format!("[server]\n{}\n{mac}", site_passwd_line()))
            .unwrap()
            .resolve()
            .unwrap();
        assert!(!config.targets[0].audio);
    }

    /// The pre-negotiation format follows the engine: CD quality is what RDP is
    /// asked for, 48 kHz stereo is what a generic server is asked for.
    #[test]
    fn the_audio_source_format_is_the_engines() {
        // Without the key, which RDP is refused until its client carries sound —
        // the format is the protocol's, and is what that client will be asked for.
        let rdp = ConfigFile::parse(&format!(
            "[server]\n{}\n[[targets]]\nname = \"w\"\nprotocol = \"rdp\"\nhost = \"h\"\nusername = \"u\"\npassword = \"p\"\n",
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(rdp.targets[0].audio_source_format(), crate::audio::PCM_CD_QUALITY);

        // A generic vnc target's is the format this client asks the extension
        // for, which is the same 48 kHz stereo and needs no resampling either.
        let vnc = ConfigFile::parse(&format!(
            "[server]\n{}\n[[targets]]\nname = \"v\"\nprotocol = \"vnc\"\nhost = \"h\"\naudio = true\n",
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(vnc.targets[0].audio_source_format(), crate::vnc_audio::SOURCE_FORMAT);
        assert_eq!(crate::vnc_audio::SOURCE_FORMAT.sample_rate, 48_000);
        assert_eq!(crate::vnc_audio::SOURCE_FORMAT.bits_per_sample, 16);
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
            name = "box"
            protocol = "vnc"
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
            username = "u"
            password = "p"
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
            username = "u"
            password = "p"
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
    /// The same, on the protocol that carries sound — every audio key is refused
    /// on RDP, whose client does not have it yet.
    fn parse_audio_target(body: &str) -> anyhow::Result<AppConfig> {
        ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "t"
            protocol = "vnc"
            host = "10.0.0.5"
            {body}
            "#,
            site_passwd_line()
        ))?
        .resolve()
    }

    fn parse_target(body: &str) -> anyhow::Result<AppConfig> {
        ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "t"
            protocol = "rdp"
            username = "u"
            password = "p"
            host = "10.0.0.5"
            {body}
            "#,
            site_passwd_line()
        ))?
        .resolve()
    }

    /// The switch resolves into the plan with its default floor, on both shapes
    /// of stream — and the plan says so, beside the stream it belongs to.
    #[test]
    fn render_adaptive_resolves_a_floor_into_the_plan() {
        let cfg = parse_target("render_type = \"video\"\nvideo_quality = 80\nrender_adaptive = true")
            .expect("adaptive video");
        let plan = cfg.targets[0].render_plan(Chroma::Subsampled);
        assert_eq!(
            plan,
            RenderPlan::Video { quality: 80, adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN), chroma: Chroma::Subsampled }
        );
        assert_eq!(plan.describe(), "video q80 4:2:0 · adaptive ≥20");

        // On a motion plan the floor is the stream's, and the base beside it —
        // lossy here — keeps the quality it was configured with.
        let cfg = parse_target(
            "render_subtype = \"webp\"\nimage_quality = 70\nrender_motion = true\n\
             video_quality = 60\nrender_adaptive = true\nrender_adaptive_min = 35",
        )
        .expect("adaptive motion stream");
        let plan = cfg.targets[0].render_plan(Chroma::Subsampled);
        assert_eq!(
            plan,
            RenderPlan::Tiles {
                base: TileCodec::Webp { quality: 70 },
                motion: Some(MotionEncode {
                    quality: 60,
                    adaptive: Some(35),
                    chroma: Chroma::Subsampled
                }),
                debug: false,
            }
        );
        assert_eq!(
            plan.describe(),
            "motion · base webp q70, moving stream q60 4:2:0 · adaptive ≥35"
        );
    }

    /// A dial below the default floor takes the floor down with it, on both shapes
    /// of stream. The walk is the operator's, and the widest one a dial of 10 admits
    /// runs from 10 to 10 — not from 20, which is a quality that stream never sends.
    /// The card has to say the same, or it promises a floor nothing walks down to.
    ///
    /// Only the default reaches here: a written `render_adaptive_min` above the dial
    /// is refused at parse ([`a_floor_above_a_ceiling_is_refused`]).
    #[test]
    fn a_dial_below_the_default_floor_is_the_floor() {
        let cfg = parse_target("render_type = \"video\"\nvideo_quality = 10").expect("a low dial");
        let plan = cfg.targets[0].render_plan(Chroma::Subsampled);
        assert_eq!(
            plan,
            RenderPlan::Video { quality: 10, adaptive: Some(10), chroma: Chroma::Subsampled }
        );
        assert_eq!(plan.describe(), "video q10 4:2:0 · adaptive ≥10");

        let cfg = parse_target("render_motion = true\nvideo_quality = 10").expect("a low dial");
        assert_eq!(
            motion_of(cfg.targets[0].render_plan(Chroma::Subsampled)),
            Some(MotionEncode { quality: 10, adaptive: Some(10), chroma: Chroma::Subsampled })
        );
    }

    /// A target that turned the walk off stays exactly on its dial: no floor in the
    /// plan, and the pressure-only walk the streams had before the key existed.
    #[test]
    fn render_adaptive_false_leaves_the_plan_without_a_floor() {
        let cfg = parse_target("render_type = \"video\"\nvideo_quality = 80\nrender_adaptive = false")
            .expect("video with the walk off");
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan::Video { quality: 80, adaptive: None, chroma: Chroma::Subsampled }
        );
        assert_eq!(cfg.targets[0].render_plan(Chroma::Subsampled).describe(), "video q80 4:2:0");
    }

    /// And a target that said nothing gets the walk anyway: it is on, at its own
    /// floor, under the quality the operator did not have to write either.
    #[test]
    fn the_walk_and_its_dial_are_a_streaming_targets_default() {
        let cfg = parse_target("render_type = \"video\"").expect("bare video");
        let plan = cfg.targets[0].render_plan(Chroma::Subsampled);
        assert_eq!(
            plan,
            RenderPlan::Video {
                quality: DEFAULT_VIDEO_QUALITY,
                adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                chroma: Chroma::Subsampled
            }
        );
        assert_eq!(plan.describe(), "video q90 4:2:0 · adaptive ≥20");
    }

    /// The walk is a stream's. A target that sends only tiles has none, whether
    /// its tiles are lossless or lossy: a still is sent once, and one the link
    /// coarsened would have nothing coming back for it. Both ways of writing the
    /// key are refused there — turning off a walk that never runs says as little
    /// as turning it on, and the walk being the default is not a reason to let a
    /// tiles target opt out of it.
    #[test]
    fn render_adaptive_is_refused_where_nothing_streams() {
        for body in [
            "render_adaptive = true",
            "render_adaptive = false",
            "render_subtype = \"webp\"\nimage_quality = 70\nrender_adaptive = true",
            "render_subtype = \"classify\"\nimage_quality = 70\nrender_adaptive = false",
        ] {
            let err = parse_target(body).unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains("render_adaptive"), "{body}: {rendered}");
            assert!(rendered.contains("sends none"), "{body}: {rendered}");
        }
        // And so is the floor, which the switch is no longer needed to reach.
        let err = parse_target("render_adaptive_min = 30").unwrap_err();
        assert!(format!("{err:#}").contains("no stream to walk"), "{err:#}");
    }

    /// The floor belongs to the walk; beside a walk that was turned off nothing
    /// reads it.
    #[test]
    fn render_adaptive_min_beside_a_walk_turned_off_is_refused() {
        let err = parse_target(
            "render_type = \"video\"\nvideo_quality = 80\n\
             render_adaptive = false\nrender_adaptive_min = 30",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("render_adaptive_min"));
    }

    /// A floor above the stream's quality leaves the walk nowhere to go — and the
    /// stream's is the only ceiling: a base's `image_quality` never walks, so a
    /// floor above it contradicts nothing.
    #[test]
    fn a_floor_above_a_ceiling_is_refused() {
        let err = parse_target(
            "render_type = \"video\"\nvideo_quality = 50\n\
             render_adaptive = true\nrender_adaptive_min = 60",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to go"));

        let err = parse_target(
            "render_motion = true\n\
             video_quality = 10\nrender_adaptive = true\nrender_adaptive_min = 30",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to go"));

        // The *default* floor over the same low dial is no contradiction — the
        // operator never wrote it. It parses, and [`TargetConfig::render_plan`]
        // resolves the floor to the dial instead
        // ([`a_dial_below_the_default_floor_is_the_floor`]).
        parse_target(
            "render_motion = true\n\
             video_quality = 10\nrender_adaptive = true",
        )
        .expect("a default floor clamps instead of refusing");

        parse_target(
            "render_subtype = \"webp\"\nimage_quality = 30\nrender_motion = true\n\
             video_quality = 60\nrender_adaptive = true\nrender_adaptive_min = 40",
        )
        .expect("a floor is measured against the stream, not the base");
    }

    /// The audio keys resolve the same way the render dial does: defaults
    /// filled, kilobits become bits, and the floor is present exactly when the
    /// walk was asked for.
    #[test]
    fn the_audio_plan_resolves_defaults_and_the_adaptive_floor() {
        let cfg = parse_audio_target("audio = true").expect("bare audio");
        assert_eq!(cfg.targets[0].audio_plan(), AudioPlan::default());
        assert_eq!(cfg.targets[0].audio_plan().bitrate_bps, 96_000);

        let cfg = parse_audio_target("audio = true\naudio_bitrate = 128").expect("a rate");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan { codec: AudioCodec::Opus, bitrate_bps: 128_000, adaptive_floor_bps: None }
        );

        let cfg = parse_audio_target("audio = true\naudio_adaptive = true").expect("adaptive");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan {
                codec: AudioCodec::Opus,
                bitrate_bps: 96_000,
                adaptive_floor_bps: Some(32_000)
            }
        );

        let cfg = parse_audio_target(
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
        let err = parse_audio_target(
            "audio = true\naudio_codec = \"pcm\"\naudio_bitrate = 96",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("audio_bitrate"));

        let err = parse_audio_target(
            "audio = true\naudio_codec = \"pcm\"\naudio_adaptive = true",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("audio_adaptive"));

        // And without audio at all, same rule one step up.
        let err = parse_audio_target("audio_bitrate = 96").unwrap_err();
        assert!(format!("{err:#}").contains("audio_bitrate"));
    }

    /// The floor needs the walk, has a range, and must sit under the ceiling.
    #[test]
    fn the_audio_floor_is_validated_against_the_walk_and_the_ceiling() {
        let err = parse_audio_target("audio = true\naudio_bitrate_min = 24").unwrap_err();
        assert!(format!("{err:#}").contains("audio_adaptive"));

        let err = parse_audio_target(
            "audio = true\naudio_adaptive = true\naudio_bitrate_min = 4",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("6–510"));

        let err = parse_audio_target(
            "audio = true\naudio_bitrate = 48\naudio_adaptive = true\naudio_bitrate_min = 48",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to go"));

        // The *default* floor above a low ceiling is no contradiction — the
        // operator never wrote it. It parses, and the walk clamps it to the
        // ceiling (`AudioCongestion::new`) instead.
        parse_audio_target("audio = true\naudio_bitrate = 8\naudio_adaptive = true")
            .expect("a default floor clamps instead of refusing");

        let err = parse_audio_target("audio = true\naudio_bitrate = 999").unwrap_err();
        assert!(format!("{err:#}").contains("6–510"));
    }
}
