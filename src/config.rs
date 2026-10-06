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
use serde::{Deserialize, Serialize};

use crate::audio::PcmFormat;
#[cfg(feature = "embedded-gateway")]
use crate::auth::EmbeddedToken;
use crate::auth::{GatewayAuth, SitePasswd};
use crate::protocol::HostDisplay;
use crate::throughput::MeterConfig;

/// Remote-desktop protocol of a target. Each has a server-side engine feeding
/// the same browser protocol (docs/architecture.md): `rdp` via the built-in RDP
/// client (src/rdp.rs over src/rdp_client), `vnc` via the built-in RFB client (src/vnc.rs). A Mac is reached
/// with an Apple [`Subtype`], over Apple's own RFB 003.889.
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
/// business; every current subtype is `vnc`'s: two describe the same Mac, and one
/// is wlshare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Subtype {
    /// macOS Screen Sharing in Standard mode, on the wire Apple's viewer uses for
    /// every Mac: RFB 003.889, its AES-128-CBC record layer (see
    /// [`crate::vnc_record`]) and Apple's control messages (see
    /// [`crate::vnc_apple`]), authenticated the way Apple Remote Desktop does: the
    /// credentials are a *macOS account's* and the connection is named to the Mac,
    /// which is what makes it share the screen rather than a login window of its
    /// own (see [`crate::vnc`]). A third-party VNC server that happens to run on a
    /// Mac is not this — it is a plain `vnc` target.
    ///
    /// The Mac's metadata extension lists every attached display, permits selecting
    /// one or their combined desktop, and supplies each display's pixel density.
    /// Apple's native pasteboard is available, and the rectangles are ZRLE.
    ///
    /// With the unofficial [`TargetConfig::virtual_display`], the same session
    /// opens on one of the Mac's virtual displays instead — High Performance's
    /// display and resizing under Standard's picture, a combination Apple's viewer
    /// never offers and remotex tested against macOS 26 alone.
    Ard,
    /// The same Mac in High Performance Screen Sharing, as Apple's viewer has it:
    /// the same wire as [`Subtype::Ard`] on a virtual display, with the picture as
    /// HEVC and the sound as AAC-ELD over the media
    /// stream Screen Sharing negotiates on the RFB connection and sends over UDP
    /// with SRTP ([`crate::vnc_apple_media`]). ZRLE rectangles are stepped over
    /// unread and never shown; the browser says the screen is not available
    /// until the media stream sends the display's first picture. A stream that
    /// fails ends the session, as it ends Apple's viewer's.
    ///
    /// None of this is documented by Apple: the revision, its record layer and its
    /// control messages, which it shares with [`Subtype::Ard`], its virtual display
    /// handling and its media stream were all reverse engineered, and are only as
    /// correct as the Macs they have been measured against — docs/apple-vnc-889.md
    /// records which, and what is still inferred. A macOS update is free to change
    /// any of it.
    ///
    /// High Performance Screen Sharing uses a virtual display rather than the
    /// Mac's physical displays. This gateway requests one virtual display at the
    /// session's opening size ([`TargetConfig::opening_size`]). Apple's native
    /// pasteboard payloads are carried inside the encrypted record transport.
    /// In a session that follows the window,
    /// viewport reports replace the virtual display's one advertised mode and the
    /// Mac answers with its new layout.
    ///
    /// The picture and the sound go together — the Mac refuses one without the
    /// other, and mutes its own output while the sound leg runs — so a session
    /// always carries sound and the picker offers no choice of it. The sound goes
    /// to the browser as the Mac's own AAC-ELD, never decoded here. The picture's
    /// decoder is loaded from the system when it is needed; a gateway whose host
    /// lacks it can only pass the picture ([`Passthrough::AppleMedia`]), to a
    /// browser that decodes it.
    ArdHighPerformance,
    /// [wlshare](https://github.com/andrewtheguy/wlshare), our own wlroots VNC
    /// server, spoken to as what it is: RFB 3.8 with wlshare's private extensions
    /// listed. Its picture is its own VP9 stream, passed to the browser untouched
    /// ([`crate::vnc`]), the output's pixel density and the compositor's output
    /// list come over extensions of their own, and a session started with sound,
    /// [`TargetConfig::camera`] and [`TargetConfig::microphone`] ask for the
    /// extensions that carry them.
    ///
    /// The subtype is what lists any of it. A plain `vnc` target pointed at the
    /// same server lists none, and wlshare serves it as it serves any VNC client:
    /// ZRLE, encoded here, at 1x, on the one output it opened with and without
    /// sound. Credentials are a plain target's.
    Wlshare,
}

impl Subtype {
    /// The name as written in the config file.
    pub fn name(self) -> &'static str {
        match self {
            Subtype::Ard => "ard",
            Subtype::ArdHighPerformance => "ard-high-performance",
            Subtype::Wlshare => "wlshare",
        }
    }

    /// Whether the picture and sound come over the media stream.
    pub fn media_stream(self) -> bool {
        match self {
            Subtype::Ard | Subtype::Wlshare => false,
            Subtype::ArdHighPerformance => true,
        }
    }

    /// Whether this subtype is a Mac's: Apple's RFB 003.889, authenticated the
    /// Apple Remote Desktop way (RFB security type 30), which is what makes the
    /// credentials a macOS account's. Neither a plain `vnc` target nor a
    /// `wlshare` one is.
    pub fn apple(self) -> bool {
        match self {
            Subtype::Ard | Subtype::ArdHighPerformance => true,
            Subtype::Wlshare => false,
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
    /// The trade is the decoder. Hardware that decodes profile 1 exists, but no
    /// browser's hardware VP9 path takes it, so in a browser this always decodes
    /// in software — Chromium does (measured headless, 2026-09-01),
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

/// The encoder's own word for it: the config's chroma is a key and a wire answer, the
/// crate's is a VP9 profile, and this is the one place the first becomes the second.
impl From<Chroma> for screen_vp9::Chroma {
    fn from(chroma: Chroma) -> Self {
        match chroma {
            Chroma::Subsampled => Self::Subsampled,
            Chroma::Full => Self::Full,
        }
    }
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
    /// The Opus target bitrate — the average the encoder holds to, and the
    /// ceiling of the walk when the plan is adaptive.
    pub bitrate_bps: i32,
    /// Whether the bitrate tracks the audio socket's backpressure, walking
    /// between sound-opus's floor and [`Self::bitrate_bps`] — and silence is
    /// shed while the link is behind. See [`TargetConfig::audio_adaptive`].
    pub adaptive: bool,
}

impl AudioPlan {
    /// The default rate with no walk — what `audio_adaptive = false` resolves to.
    pub fn fixed() -> Self {
        Self { adaptive: false, ..Self::default() }
    }
}

impl Default for AudioPlan {
    /// What an unset dial means: Opus at the default rate, walking down to the
    /// floor when the link is behind. The fallback [`crate::session`] uses when
    /// no target is selected, where there is no config to read.
    fn default() -> Self {
        Self { bitrate_bps: DEFAULT_AUDIO_BITRATE_KBPS as i32 * 1000, adaptive: true }
    }
}

/// The render choices an engine sees: the target's resolved VP9 plan and the
/// passthrough the session was started with, from [`TargetConfig::render_plan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderPlan {
    /// The 1–100 dial the stream holds on a link that can carry it, rather than a
    /// quantizer: turning that into one is [`crate::vp9`]'s business, and it is the
    /// only module that should know what a quantizer is.
    pub quality: u8,
    /// Whether the quality walk listens to the client's lag
    /// ([`TargetConfig::render_adaptive`]). Off, the congestion walk keeps its
    /// historical shape: pressure only.
    pub adaptive: bool,
    /// [`TargetConfig::render_chroma`], resolved.
    pub chroma: Chroma,
    /// The Mac's picture passes as it came, its HEVC rather than VP9 encoded
    /// here: [`Passthrough::AppleMedia`], chosen at the picker. None of the fields
    /// above reach such a picture.
    pub apple_media: bool,
    /// An RDP host's graphics pipeline passes as it came, for the browser to
    /// compose, rather than composed here and encoded as VP9:
    /// [`Passthrough::RdpGraphics`], chosen at the picker. The fields above reach
    /// only the picture of a host that answers the offer of the pipeline with
    /// bitmap updates, which is encoded here as always.
    pub rdp_graphics: bool,
    /// The host may draw with H.264 on that passed pipeline, for the browser to
    /// decode: [`TargetConfig::egfx_h264`], in a session that passes the pipeline
    /// to a browser that decodes it ([`Decoders::rdp_h264`]). Never set without
    /// [`Self::rdp_graphics`].
    pub rdp_h264: bool,
}

/// A remote's own stream, passed to the browser as it came instead of decoded
/// here and encoded as VP9. Which one a target has to pass is its type's to say
/// ([`TargetConfig::offers`]); whether a session passes it is chosen at the picker
/// ([`Choices::passthrough`]).
///
/// For a LAN either way: the stream is the remote's own, with no quality walk
/// behind it, so [`TargetConfig::video_quality`], [`TargetConfig::render_chroma`]
/// and the adaptive keys do not reach a passed picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Passthrough {
    /// An RDP host's graphics pipeline, its commands out of their bulk compression,
    /// for the page to compose. Offered by an `rdp` target with its pipeline on.
    ///
    /// The pipeline is drawn against what the client already holds — its surfaces,
    /// its cache slots, each codec's own caches — so a browser that comes back has
    /// nothing a running session can be resumed onto: a reattach reconnects the
    /// host instead.
    ///
    /// The compositor the page runs is the gateway's own and is
    /// unit tested as it is there, and what is passed is checked against a real
    /// host, by the probe, by a headless browser and by hand. That is a physical
    /// Windows 11 computer, from a desktop browser and from a mobile browser on iOS, with sound
    /// and the clipboard beside it;
    /// [`TargetConfig::camera`] and [`TargetConfig::microphone`] beside it have not
    /// been tried.
    RdpGraphics,
    /// A High Performance Mac's picture: its HEVC instead of VP9 encoded here from
    /// decoded pictures. Offered by `ard-high-performance`. The Mac's sound is not
    /// part of the choice: its AAC-ELD is passed in every session.
    AppleMedia,
}

impl Passthrough {
    /// The name as the browser reads it, on `/api/targets` and in a session's
    /// status.
    pub fn name(self) -> &'static str {
        match self {
            Self::RdpGraphics => "rdp-graphics",
            Self::AppleMedia => "apple-media",
        }
    }

    /// What is passed, for a card and for a refusal.
    pub fn stream(self) -> &'static str {
        match self {
            Self::RdpGraphics => "the host's graphics pipeline",
            Self::AppleMedia => "the Mac's HEVC",
        }
    }
}

/// Which choices the picker shows under a target: what its type has to offer, from
/// [`TargetConfig::offers`]. One that is not offered has no row there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offers {
    /// Whether the window can drive the desktop's size ([`Sizing::Window`]).
    pub resize: bool,
    /// Whether the remote's sound is a choice. False both where there is none to
    /// take and where it is always carried, as on `ard-high-performance`.
    pub audio: bool,
    /// The stream this target can pass untouched, where it has one.
    pub passthrough: Option<Passthrough>,
    /// Whether where the second virtual display sits is a choice
    /// ([`Placement`]): on a target that asks for two of a host that is told
    /// where each is.
    pub placement: bool,
}

/// What whoever started a session chose under its target at the picker, carried by
/// [`crate::protocol::ClientMsg::Connect`]. They hold for the life of the session:
/// the slot keeps them beside the target, and no other browser is given them.
///
/// Each is refused on a target that does not offer it
/// ([`TargetConfig::accepts`]). The size is always named, since which one a
/// session has is the browser's to say and never this end's to assume; sound and
/// passthrough are taken only where they are named.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Choices {
    /// How the desktop is sized.
    pub size: Sizing,
    /// Whether the remote's sound is taken, and what it is sent to the browser
    /// as. RDP negotiates it at connect (MS-RDPEA); a `wlshare` target lists
    /// wlshare's audio extension, Opus or FLAC on the RFB connection
    /// ([`crate::vnc_audio`]). At [`Sound::Off`] neither is asked, so the host keeps
    /// playing where it did. Packets are sent only while the attached browser
    /// subscribes, which is what its Mute and Unmute change.
    #[serde(default)]
    pub audio: Sound,
    /// Pass the target's [`Passthrough`].
    #[serde(default)]
    pub passthrough: bool,
    /// Where the second virtual display sits.
    #[serde(default)]
    pub placement: Placement,
}

impl Choices {
    /// Whether the client's window drives the desktop's size.
    pub fn resize(self) -> bool {
        self.size == Sizing::Window
    }
}

/// Where the second of two virtual displays sits against the first:
/// [`Choices::placement`], on a target that offers it ([`Offers::placement`]).
///
/// An RDP host is told each monitor's position, in the connect-time monitor data
/// and in every layout after, so it arranges the two as asked: a window dragged
/// over that edge of the first display arrives on the second. Beside the first
/// the two are top-aligned, and above or below it left-aligned. A High
/// Performance Mac places its second display itself, on the right, and offers
/// no choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    #[default]
    Right,
    Left,
    Top,
    Bottom,
}

impl Placement {
    /// Where the second monitor's corner is against the first's, the first being
    /// `first` in size and the second `second`: what a monitor's position is
    /// stated relative to ([MS-RDPBCGR] 2.2.1.3.6.1, [MS-RDPEDISP] 2.2.2.2.1).
    pub fn second_corner(self, first: (u32, u32), second: (u32, u32)) -> (i32, i32) {
        let signed = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        match self {
            Self::Right => (signed(first.0), 0),
            Self::Left => (-signed(second.0), 0),
            Self::Top => (0, -signed(second.1)),
            Self::Bottom => (0, signed(first.1)),
        }
    }

    /// The desktop two monitors of these sizes make: their union.
    pub fn union(self, first: (u32, u32), second: (u32, u32)) -> (u32, u32) {
        match self {
            Self::Right | Self::Left => (first.0.saturating_add(second.0), first.1.max(second.1)),
            Self::Top | Self::Bottom => (first.0.max(second.0), first.1.saturating_add(second.1)),
        }
    }
}

/// Whether a session takes the remote's sound, and what that sound is sent to the
/// browser as: [`Choices::audio`], on a target that offers it ([`Offers::audio`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Sound {
    /// The remote is asked for none.
    #[default]
    Off,
    /// At the rate the audio keys hold: an RDP host's PCM encoded here, or
    /// wlshare's own Opus packets, coded there at that rate, passed as they came.
    Opus,
    /// Lossless: wlshare's own FLAC frames passed as they came, or
    /// an RDP host's PCM coded as FLAC here by libFLAC, decoded by the page's
    /// WebAssembly module either way. The uncompressed rate less a third or so,
    /// about a megabit a second of music, with no walk under it: for a link with
    /// room.
    Flac,
}

/// How a session's desktop is sized: at a size it keeps, or by the client's
/// window. The picker shows the size a session will have before Start.
///
/// Which are offered depends on the target and on the client. A target the
/// window cannot drive ([`Offers::resize`]) has [`Self::Target`] alone. One it
/// can drive offers a client with a window to follow, a desktop browser or a
/// tablet, [`Self::Window`], and beside it [`Self::Target`] where the operator
/// configured a size; it offers a phone, whose window is no desktop's shape,
/// [`Self::Target`] and, where a size is configured, [`Self::BuiltIn`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Sizing {
    /// The target's size, kept for the session: its configured
    /// [`TargetConfig::size`], or [`DEFAULT_SIZE`] where it has none.
    #[default]
    Target,
    /// [`DEFAULT_SIZE`], kept for the session, on a target that configures
    /// another.
    #[serde(rename = "default")]
    BuiltIn,
    /// The client's window drives the size, and the configured one is not used. A
    /// desktop client reports every window change; a tablet asks once for its
    /// screen. There is no client-side mode or manual resize command beside it.
    ///
    /// On RDP this also turns on density matching, because there a density *is* a
    /// resize: the Display Control channel this negotiates is the only way to tell
    /// a live session to render at 200%, so a Retina client gets twice the pixels
    /// and a UI drawn twice as large. At a kept size an RDP session ignores a
    /// pointer client's density, and states a pinch-zoom client's once at connect.
    /// An RDP resize is the graphics pipeline's, so a
    /// target with `egfx = false` does not offer it.
    ///
    /// On a virtual display — `ard-high-performance`, or `ard` with
    /// [`TargetConfig::virtual_display`] — the setup descriptor always enables the
    /// Mac's dynamic geometry; this decides only whether the window keeps driving
    /// it after the open. Standard `ard` without one does not offer it, because it
    /// exposes physical displays. Neither does a plain `vnc` target: whether its
    /// server accepts a size is known only after it is dialled, which is too late
    /// for a picker to offer it.
    Window,
}

/// A choice made for a target whose type does not offer it.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("target {target:?} does not offer {choice}")]
pub struct NotOffered {
    pub target: String,
    pub choice: &'static str,
}

/// What the attached browser said it can take, from its session socket
/// ([`crate::ws`]): the questions the page asks once at load and states on every
/// session socket it opens. The chroma *selects* a stream. The two after it say which
/// passthrough this browser can be served, which is what the picker greys a choice
/// by and what ends a session whose owner comes back unable to take its own
/// ([`TargetConfig::beyond`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoders {
    /// The most colour it takes, which resolves [`ChromaChoice::Auto`].
    pub chroma: Chroma,
    /// Whether it decodes a High Performance Mac's picture: the HEVC, Range
    /// Extensions 4:4:4 ([`Passthrough::AppleMedia`]).
    pub apple_media: bool,
    /// Whether it composes an RDP host's graphics pipeline
    /// ([`Passthrough::RdpGraphics`]): the page's compositor needs shared memory,
    /// so a cross-origin isolated page, and a WebGL 2 canvas to present on.
    pub rdp_graphics: bool,
    /// Whether it decodes the H.264 an RDP host may draw with on that pipeline
    /// ([`TargetConfig::egfx_h264`]): a `VideoDecoder` that takes it and hands its
    /// pictures into the compositor's memory. Unlike the two above it turns no
    /// session away: a browser that says no is passed a pipeline without H.264.
    pub rdp_h264: bool,
}

impl Decoders {
    /// Whether this browser can be served `passthrough`.
    pub fn takes(self, passthrough: Passthrough) -> bool {
        match passthrough {
            Passthrough::AppleMedia => self.apple_media,
            Passthrough::RdpGraphics => self.rdp_graphics,
        }
    }
}

/// A browser that states `chroma` and takes every passthrough, for tests about
/// everything else a browser says.
#[cfg(test)]
impl From<Chroma> for Decoders {
    fn from(chroma: Chroma) -> Self {
        Self { chroma, apple_media: true, rdp_graphics: true, rdp_h264: true }
    }
}

impl RenderPlan {
    /// The stream this plan passes untouched, if any.
    pub fn passthrough(&self) -> Option<Passthrough> {
        if self.apple_media {
            Some(Passthrough::AppleMedia)
        } else if self.rdp_graphics {
            Some(Passthrough::RdpGraphics)
        } else {
            None
        }
    }

    /// This plan in one line, for the client's session card.
    ///
    /// The resolved plan rather than the config keys: what a target *does* is the
    /// plan its keys collapse to, defaults and the browser's chroma included, and a
    /// description built from the keys would restate the file while the encoder did
    /// something the reader has to derive.
    pub fn describe(&self) -> String {
        self.card(None)
    }

    /// [`Self::describe`] with the chroma slot said differently — the one thing a
    /// reader without a browser knows better than a resolved plan does, because
    /// [`ChromaChoice::Auto`] has nothing here to resolve against. See
    /// [`TargetConfig::render_summary`], the only caller that passes anything.
    fn card(&self, chroma_slot: Option<&str>) -> String {
        if let Some(passthrough) = self.passthrough() {
            let h264 = if self.rdp_h264 { " with H.264" } else { "" };
            return format!("{}{h264}, passed through", passthrough.stream());
        }
        // Always named, because with `auto` the default there is no chroma a card
        // may leave unsaid: an unnamed one would read as 4:2:0 selected on a
        // session that is 4:2:0 only because this browser declined profile 1. What
        // the slot says is the profile on the wire — or, for a config card,
        // whatever `chroma_slot` puts there instead.
        let chroma = match chroma_slot {
            Some(slot) => slot.to_owned(),
            None => self.chroma.card_name().to_owned(),
        };
        // The walk as a suffix: the quality named before it is a ceiling the link
        // may fall below.
        let adaptive = if self.adaptive { " · adaptive" } else { "" };
        format!("video q{} {chroma}{adaptive}", self.quality)
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
    /// Optional domain of an RDP account. Refused on any other protocol, which has
    /// nowhere to send it.
    #[serde(default)]
    pub domain: Option<String>,
    /// The size the desktop is kept at, in points, written as width by height:
    /// `size = "1920x1080"`. Optional: a target without one keeps
    /// [`DEFAULT_SIZE`] instead. A session that follows the client's window
    /// ([`Sizing::Window`]) does not use it — see [`Self::opening_size`].
    ///
    /// How a kept size is stated depends on the engine: RDP connects at it, a
    /// Mac's virtual display is created at it, and a plain or wlshare VNC server
    /// is asked for it with one `SetDesktopSize`, as soon as it declares support
    /// (`Flags::kept` in src/vnc.rs) — a server that never does, or refuses,
    /// keeps its own. Standard `ard` shares the Mac's physical displays, which
    /// this gateway never resizes, so the key is refused there
    /// ([`ConfigFile::parse`]); with [`Self::virtual_display`] it sizes the
    /// virtual display, as on High Performance.
    ///
    /// Points rather than pixels, because the density can move underneath it:
    /// a High Performance Mac opens at the client's density whatever the size,
    /// and a size has to keep meaning the same desktop rather than half of one.
    /// See `Density` in src/rdp.rs.
    #[serde(default, deserialize_with = "size")]
    pub size: Option<(u16, u16)>,
    /// UNOFFICIAL. Open Standard mode (`subtype = "ard"`) on one virtual display
    /// instead of the Mac's physical displays: the `SetDisplayConfiguration` High
    /// Performance sends, with Standard's ZRLE picture and no media stream. The
    /// display and its resizing are High Performance's, everything else — the
    /// picture, the absence of sound, the pasteboard — is `ard`'s. It is a
    /// combination Apple's viewer never offers, so no Apple client exercises the
    /// Mac's side of it; it was tested against macOS 26 only, and a macOS update
    /// is free to break it. Refused on `ard-high-performance`, which always has
    /// the display, and on every target that is not a Mac.
    ///
    /// What it is for: a resizable Mac session, at the window's size and density,
    /// in a gateway without the Mac's decoders' libraries or on a Mac where the
    /// media stream cannot reach it.
    #[serde(default)]
    pub virtual_display: bool,
    /// RDP's graphics pipeline (MS-RDPEGFX), on by default. On, a Windows host
    /// draws the desktop through the pipeline's surfaces and marks every frame,
    /// and a resize is a graphics reset. Off, the host draws with bitmap updates
    /// and the desktop keeps its opening size — the picker then offers neither
    /// resize nor the pipeline's passthrough; that is the escape hatch for a host
    /// whose pipeline this client's decoders cannot yet paint, and the path every
    /// non-Windows server takes regardless.
    ///
    /// `Option` rather than a bare default so that setting it on a VNC target,
    /// which has no graphics pipeline to switch, is refused at parse time
    /// instead of accepted and left inert; `None` reads as on
    /// ([`TargetConfig::egfx`]).
    #[serde(default)]
    pub egfx: Option<bool>,
    /// EXPERIMENTAL. Let the host draw with H.264 on a pipeline that is passed
    /// ([`Passthrough::RdpGraphics`]), for the browser to decode. Refused on a
    /// target that is not RDP and beside `egfx = false`, which have no pipeline to
    /// carry it.
    ///
    /// A Windows host told its client takes H.264 hands the parts of the desktop
    /// that move like video — a player, a scrolling page — to it, and goes on
    /// drawing the rest with the lossless codecs in the same frames. That is the
    /// host's own trade of detail for bitrate on those parts, which is why it is a
    /// key and off unless set: without it every passed pipeline is lossless.
    ///
    /// It reaches only a session started with the passthrough, and only a browser
    /// that said it decodes H.264 ([`Decoders::rdp_h264`]); every other session of
    /// the target is told what it always was, that the client takes none. The
    /// gateway decodes nothing: the access units ride in the commands it passes,
    /// and the page decodes them with the browser's `VideoDecoder` and paints them
    /// through its compositor ([`remotex_rdp_graphics::avc`]).
    ///
    /// A key rather than a choice at the picker while it is experimental. Checked
    /// against one Windows 11 host without a GPU, whose stream is Main profile and
    /// AVC420 by region; AVC444, which a host policy may select, is implemented
    /// from the specification and has not been seen from a host.
    #[serde(default)]
    pub egfx_h264: bool,
    /// ALPHA. How many virtual displays the remote is asked to lay out for the
    /// session, side by side at the session's size: `virtual_displays = 2`.
    /// One, the default, is the single desktop every target has always opened;
    /// at most [`MAX_VIRTUAL_DISPLAYS`].
    ///
    /// The browser shows one of them at a time, chosen from the display picker
    /// in the floating menu, and the gateway encodes only the one shown: each
    /// display is held under the stream's ceiling on its own, and the host
    /// renders the other for the windows left on it. The key is a count the
    /// remote is *asked* for; the list the picker shows is what the remote laid
    /// out, so a server that opens one desktop shows no picker.
    ///
    /// Shared by every target type that can create virtual displays: `rdp` and
    /// `ard-high-performance`. A Windows host lays the displays out from the
    /// connect-time monitor data ([MS-RDPBCGR] 2.2.1.3.6) and from each monitor
    /// layout a resizing session sends ([MS-RDPEDISP] 2.2.2.2), as one desktop
    /// spanning both. Passed through, that desktop is composed once in the
    /// browser, which shows one display of it and paints the second display's
    /// tab from the same picture ([`crate::protocol::ServerMsg::GraphicsView`]).
    /// A Mac in High Performance mode creates each
    /// from its `SetDisplayConfiguration` descriptor and sends each as a media
    /// stream of its own, which is Apple's viewer's "2 Virtual Displays", passed
    /// through or not. Refused on every other target, where it would be silently
    /// inert.
    ///
    /// Alpha: checked against one Windows 11 host and one Mac, a virtual one; the
    /// second display's density follows the first's, and nothing of it has been
    /// measured against Microsoft's or Apple's own client.
    #[serde(default = "one_display")]
    pub virtual_displays: u8,
    /// Offer the remote a redirected camera: MS-RDPECAM on RDP, and on a
    /// `wlshare` target the wlshare camera extension ([`crate::vnc_camera`]),
    /// listed the way its audio extension is. Rejected on a plain `vnc`
    /// target and on both Apple subtypes: neither speaks such an extension.
    ///
    /// **Experimental.** The socket's session rules and message encodings are
    /// unit tested, both wires are checked, and the wlshare path has container
    /// coverage. The RDP redirection itself is exercised only against a real
    /// Windows host, because only a host that creates the
    /// `RDCamera_Device_Enumerator` channel — a workstation, or a Windows Server
    /// carrying the Remote Desktop Session Host role — has anywhere to redirect
    /// a camera to.
    ///
    /// Capability only. The device itself appears when a client enables the
    /// camera — explicitly, per session, never remembered — by opening
    /// `/ws/camera`; a target with this key and no such client offers the
    /// remote nothing. The browser encodes H.264 and the gateway passes it
    /// through, so there is no codec key beside this one.
    #[serde(default)]
    pub camera: bool,
    /// Offer the remote this browser's microphone: MS-RDPEAI on RDP, and on a `wlshare`
    /// target the wlshare microphone extension ([`crate::vnc_mic`]), listed the way
    /// [`Self::camera`]'s is. Rejected on a plain `vnc` target and on both Apple
    /// subtypes: neither speaks such an extension.
    ///
    /// Capability only, like [`Self::camera`]: the recording device is fed when a client
    /// enables its microphone — explicitly, per session — by opening `/ws/mic`. The
    /// browser sends speech-grade Opus and the gateway decodes it to the PCM the host
    /// records in, so there is no codec or quality key beside this one.
    #[serde(default)]
    pub microphone: bool,
    /// The Opus bitrate this target's sound holds on a link that can carry it,
    /// in kbit/s (6–510); `None` reads as [`DEFAULT_AUDIO_BITRATE_KBPS`].
    ///
    /// The audio dial's `video_quality`: a *ceiling* rather than a promise. It
    /// is the average the encoder holds to — Opus is variable-rate, so a packet
    /// of silence costs a few bytes and a packet of music costs about this —
    /// and, with [`Self::audio_adaptive`] on, the rate a link that keeps up gets
    /// and the one the walk climbs back to.
    #[serde(default)]
    pub audio_bitrate: Option<u32>,
    /// Let [`Self::audio_bitrate`] track the audio socket's own backpressure —
    /// on unless the operator turned it off, like [`Self::render_adaptive`].
    ///
    /// A send that blocks means the previous packets are still unwritten, and
    /// sustained blocking walks the bitrate down toward the floor; a clear
    /// stretch walks it back up to the ceiling. The walk is sound-opus's
    /// (`sound_opus::walk`), shared with wlshare, and so is its floor, fixed
    /// there where a lower rate stops being the same sound, as the render
    /// walk's is fixed in its encoder: there is no floor key. While behind,
    /// wave buffers that are pure silence are shed instead of queued — silence
    /// is the one content whose loss is free, and dropping it is how the
    /// client catches up without a trimmed or resampled note anywhere (see
    /// [`crate::audio`]). On a `wlshare` target the walk is the same and the
    /// encoder is wlshare's, told each rate the walk arrives at; its packets
    /// are passed, so no silence is shed.
    ///
    /// Resolved by the accessor of the same name.
    #[serde(default)]
    pub audio_adaptive: Option<bool>,
    /// The quality (1–100) this target's VP9 stream holds on a link that can carry
    /// it. `None` reads as [`DEFAULT_VIDEO_QUALITY`].
    ///
    /// A ceiling rather than a promise: a link that cannot hold it coarsens until
    /// it can, and one with room to spare never earns better.
    #[serde(default)]
    pub video_quality: Option<u8>,
    /// Chroma sampling of this target's video stream; `None` reads as
    /// [`ChromaChoice::Auto`], which is every browser getting the most colour its
    /// own decoder takes.
    ///
    /// Written down only to take that decision away from the browser: `"444"` sends
    /// profile 1 to a decoder that refuses it by name, `"420"` sends the subsampled
    /// stream to one that would have taken the colour. Both are the right key for a
    /// measurement or for a fleet held to one bitstream, and the wrong one for a
    /// target watched from more than one kind of browser. See [`ChromaChoice`] and
    /// [`Self::render_plan`].
    #[serde(default)]
    pub render_chroma: Option<ChromaChoice>,
    /// Let [`Self::video_quality`] track the measured link — on unless the
    /// operator turned it off.
    ///
    /// The configured quality stays the *ceiling* — a link with room to spare
    /// never earns a better picture than the one asked for — and the walk's floor
    /// is the encoder's own, fixed where a coarser picture stops being worth more
    /// than fewer frames, as every adaptive stream fixes it. The stream already
    /// gives quality up when queueing a frame blocks; this adds the client's own
    /// lag — how long the oldest unacknowledged paint batch has been owed, beyond
    /// the link's measured floor — as a second reason to. A desktop left coarse is
    /// sharpened at the dial once it goes quiet, which is what keeps a coarse walk
    /// from being a coarse screen.
    ///
    /// Resolved by the accessor of the same name.
    #[serde(default)]
    pub render_adaptive: Option<bool>,
}

/// The stream quality a target streams at when [`TargetConfig::video_quality`] is
/// unset — the ceiling of the adaptive walk, not a promise. High enough that a link
/// with room to spare shows a desktop worth looking at, and the walk is what takes
/// it down on one that has not.
pub const DEFAULT_VIDEO_QUALITY: u8 = 90;

/// The Opus bitrate (kbit/s) when [`TargetConfig::audio_bitrate`] is unset — the
/// ceiling of the adaptive walk, not a promise. Well clear of where stereo Opus
/// starts to be audibly lossy, and the walk is what takes it down on a link that
/// cannot carry it.
pub const DEFAULT_AUDIO_BITRATE_KBPS: u32 = 96;

/// Read [`TargetConfig::size`]: a width by a height, as in `"1920x1080"`.
fn size<'de, D>(deserializer: D) -> Result<Option<(u16, u16)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let written = String::deserialize(deserializer)?;
    written
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .map(Some)
        .ok_or_else(|| {
            serde::de::Error::custom(format!(
                "size {written:?} is not a width by a height, as in \"1920x1080\""
            ))
        })
}

/// A configured size as the config file writes it, for a refusal.
fn size_text(size: Option<(u16, u16)>) -> String {
    size.map_or_else(String::new, |(w, h)| format!("{w}x{h}"))
}

impl TargetConfig {
    /// The size a session started with `sizing` opens at, in points. One rule
    /// for every engine that can ask for an opening size, so none of them
    /// branches on its own. A kept size is also the size the session stays at.
    ///
    /// A session that follows the window opens at the full resolution of the
    /// client's own screen (named in [`crate::protocol::ClientMsg::Connect`]),
    /// which the window then replaces. A client that fits the desktop to its
    /// viewport and pinch-zooms ([`HostDisplay::fit`]) has a screen but not one
    /// to open at: it is the one client not showing the desktop at 100%, and
    /// its screen is a tablet's, to be asked for in landscape once the session
    /// is up. It opens at the default, as does a client that named no screen.
    pub fn opening_size(&self, sizing: Sizing, display: Option<HostDisplay>) -> (u16, u16) {
        match sizing {
            Sizing::Target => self.kept_size(),
            Sizing::BuiltIn => DEFAULT_SIZE,
            Sizing::Window => {
                display.filter(|d| !d.fit).map_or(DEFAULT_SIZE, |d| (d.w, d.h))
            }
        }
    }

    /// The size this target's desktop is kept at where the window does not
    /// drive it: the configured [`Self::size`], or [`DEFAULT_SIZE`].
    pub fn kept_size(&self) -> (u16, u16) {
        self.size.unwrap_or(DEFAULT_SIZE)
    }

    /// Whether a session states this target's size at all. Standard `ard` on
    /// the Mac's physical displays does not: it shows them as they are.
    pub fn sized(&self) -> bool {
        !self.apple() || self.has_virtual_display()
    }

    /// RDP's graphics pipeline switch, on unless the operator turned it off.
    pub fn egfx(&self) -> bool {
        self.egfx.unwrap_or(true)
    }

    /// The ceiling this target's stream holds to, resolved: what the operator
    /// wrote, else [`DEFAULT_VIDEO_QUALITY`].
    pub fn video_quality(&self) -> u8 {
        self.video_quality.unwrap_or(DEFAULT_VIDEO_QUALITY)
    }

    /// The adaptive walk, resolved: on unless the operator turned it off.
    pub fn render_adaptive(&self) -> bool {
        self.render_adaptive.unwrap_or(true)
    }

    /// The stream keys resolved to the one [`RenderPlan`] the engines see, so
    /// `rdp::run` / `vnc::run` need not know the config enums.
    ///
    /// `decoders` is what the attached browser said its `VideoDecoder` takes,
    /// carried on the session socket and held with its attachment
    /// ([`crate::session::SessionManager::attach`]). Its chroma is read by
    /// [`ChromaChoice::Auto`] and by nothing else: a target that names a profile
    /// gets that profile whatever this says, which is what keeps the explicit key a
    /// decision no browser can overrule. `choices` is what the session was started
    /// with, whose passthrough is this target's own ([`Self::passthrough`]).
    pub fn render_plan(&self, choices: Choices, decoders: Decoders) -> RenderPlan {
        let quality = self.video_quality();
        let adaptive = self.render_adaptive();
        let chroma = match self.render_chroma.unwrap_or_default() {
            ChromaChoice::Subsampled => Chroma::Subsampled,
            ChromaChoice::Full => Chroma::Full,
            ChromaChoice::Auto => decoders.chroma,
        };
        let passthrough = self.passthrough(choices);
        let apple_media = passthrough == Some(Passthrough::AppleMedia);
        let rdp_graphics = passthrough == Some(Passthrough::RdpGraphics);
        let rdp_h264 = rdp_graphics && self.egfx_h264 && decoders.rdp_h264;
        RenderPlan { quality, adaptive, chroma, apple_media, rdp_graphics, rdp_h264 }
    }

    /// The choices the picker shows under this target.
    pub fn offers(&self) -> Offers {
        match (self.protocol, self.subtype) {
            // An RDP resize is a graphics reset, which only the pipeline has: the
            // bitmap path keeps its opening size. MS-RDPEDISP's other answer, a
            // Deactivation-Reactivation Sequence, is left out on purpose: see
            // "Bitmap updates" in docs/rdp-client.md.
            // The passthrough is the pipeline's, however many displays the host
            // lays out: the browser composes the span the host draws and shows
            // one display of it, and paints the second display's tab from the same
            // picture (`ServerMsg::GraphicsView`).
            (Protocol::Rdp, _) => Offers {
                resize: self.egfx(),
                audio: true,
                passthrough: self.egfx().then_some(Passthrough::RdpGraphics),
                placement: self.virtual_displays > 1,
            },
            // Read as any VNC server, which carries no sound, and whose answer
            // to a size is not known until it is dialled.
            (Protocol::Vnc, None) => {
                Offers { resize: false, audio: false, passthrough: None, placement: false }
            }
            // Its VP9 is the subtype's picture and not a choice.
            (Protocol::Vnc, Some(Subtype::Wlshare)) => {
                Offers { resize: true, audio: true, passthrough: None, placement: false }
            }
            // Standard mode shares the Mac's physical displays, whose resolution
            // this gateway does not change, and never touches its sound.
            (Protocol::Vnc, Some(Subtype::Ard)) => {
                Offers { resize: self.virtual_display, audio: false, passthrough: None, placement: false }
            }
            // The sound comes with the picture, so it is not a choice, and the
            // Mac places a second virtual display itself.
            (Protocol::Vnc, Some(Subtype::ArdHighPerformance)) => Offers {
                resize: true,
                audio: false,
                passthrough: Some(Passthrough::AppleMedia),
                placement: false,
            },
        }
    }

    /// Whether every one of `choices` is this target's to offer.
    pub fn accepts(&self, choices: Choices) -> Result<(), NotOffered> {
        let offers = self.offers();
        let refused = [
            ("resize", choices.resize() && !offers.resize),
            // The default beside a configured size is a phone's alternative to
            // following a window, so it goes with a size and a window to follow.
            (
                "the default size",
                choices.size == Sizing::BuiltIn && !(offers.resize && self.size.is_some()),
            ),
            ("sound", choices.audio != Sound::Off && !offers.audio),
            ("a passthrough", choices.passthrough && offers.passthrough.is_none()),
            (
                "a place for the second display",
                choices.placement != Placement::default() && !offers.placement,
            ),
        ];
        match refused.into_iter().find(|(_, refused)| *refused) {
            Some((choice, _)) => Err(NotOffered { target: self.name.clone(), choice }),
            None => Ok(()),
        }
    }

    /// The stream a session started with `choices` passes, if any.
    pub fn passthrough(&self, choices: Choices) -> Option<Passthrough> {
        self.offers().passthrough.filter(|_| choices.passthrough)
    }

    /// The passthrough a session started with `choices` runs on that `decoders`'
    /// browser cannot take. Such a browser is not started one, and its own ends.
    pub fn beyond(&self, choices: Choices, decoders: Decoders) -> Option<Passthrough> {
        self.passthrough(choices).filter(|passthrough| !decoders.takes(*passthrough))
    }

    /// Whether a session started with `choices` carries the remote's sound: where
    /// it was chosen, and always on `ard-high-performance`, whose media stream
    /// brings it ([`crate::vnc_apple_media`]).
    pub fn sound(&self, choices: Choices) -> bool {
        self.media_stream() || (self.offers().audio && choices.audio != Sound::Off)
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
    ///
    /// A passthrough is a session's choice and not the file's, so the card is the
    /// VP9 one every target has.
    pub fn render_summary(&self) -> String {
        let slot = match self.render_chroma.unwrap_or_default() {
            ChromaChoice::Auto => Some("chroma auto"),
            ChromaChoice::Subsampled | ChromaChoice::Full => None,
        };
        let decoders = Decoders { chroma: Chroma::Subsampled, apple_media: false, rdp_graphics: false, rdp_h264: false };
        self.render_plan(Choices::default(), decoders).card(slot)
    }

    /// Whether a session started with `choices` is sent this target's sound
    /// lossless, as FLAC ([`Sound::Flac`]).
    pub fn lossless(&self, choices: Choices) -> bool {
        self.offers().audio && choices.audio == Sound::Flac
    }

    /// Whether the Opus bitrate walks with the link — on unless the operator
    /// wrote `audio_adaptive = false`.
    pub fn audio_adaptive(&self) -> bool {
        self.audio_adaptive.unwrap_or(true)
    }

    /// The audio keys collapsed to what the encoder is built from, the same way
    /// [`Self::render_plan`] collapses the render dial: defaults resolved,
    /// kilobits turned into the bits libopus speaks, and whether there is a
    /// walk. A ceiling at or under the walk's floor gets a walk of nothing
    /// rather than a refused config, as `video_quality` does. Callers gate on
    /// [`Self::sound`] — a session without sound has no plan to resolve, and
    /// neither has a Mac's passed sound, which no encoder touches.
    pub fn audio_plan(&self) -> AudioPlan {
        let bitrate_kbps = self.audio_bitrate.unwrap_or(DEFAULT_AUDIO_BITRATE_KBPS);
        AudioPlan { bitrate_bps: bitrate_kbps as i32 * 1000, adaptive: self.audio_adaptive() }
    }

    /// The one PCM format this target's sound can be in, known before the
    /// remote has said anything: what the RDP engine asks a server to redirect
    /// ([`crate::audio::PCM_CD_QUALITY`]), or what a `wlshare` target
    /// is asked to code its sound from over wlshare's audio extension
    /// ([`crate::vnc_audio::SOURCE_FORMAT`]) — the last of which this client
    /// chooses outright, since the extension leaves the format to the client. The
    /// session builds an RDP target's encoder from this when the audio socket opens
    /// before the remote's channel is up, so it has to be the source's — an encoder
    /// built for the wrong rate plays every note at the wrong pitch. Callers gate on
    /// [`Self::sound`], as with [`Self::audio_plan`].
    pub fn audio_source_format(&self) -> PcmFormat {
        match self.protocol {
            Protocol::Rdp => crate::audio::PCM_CD_QUALITY,
            Protocol::Vnc => crate::vnc_audio::SOURCE_FORMAT,
        }
    }

    /// Whether this target's picture and sound come over the Mac's media stream:
    /// `ard-high-performance` ([`crate::vnc_apple_media`]).
    pub fn media_stream(&self) -> bool {
        self.protocol == Protocol::Vnc && self.subtype.is_some_and(Subtype::media_stream)
    }

    /// Whether this target is a Mac, on either Apple subtype.
    pub fn apple(&self) -> bool {
        self.protocol == Protocol::Vnc && self.subtype.is_some_and(Subtype::apple)
    }

    /// Whether this target is a wlshare server spoken to as one: `subtype =
    /// "wlshare"`, the one target whose VNC connection lists wlshare's extensions.
    pub fn wlshare(&self) -> bool {
        self.protocol == Protocol::Vnc && self.subtype == Some(Subtype::Wlshare)
    }

    /// Whether this target's session opens one virtual display on the Mac rather
    /// than sharing its physical ones: `ard-high-performance` always, and `ard`
    /// with the unofficial [`Self::virtual_display`] key. What the VNC engine's
    /// display geometry and resizing branch on; the picture's source is
    /// [`Self::media_stream`]'s question.
    pub fn has_virtual_display(&self) -> bool {
        match (self.protocol, self.subtype) {
            (Protocol::Vnc, Some(Subtype::ArdHighPerformance)) => true,
            (Protocol::Vnc, Some(Subtype::Ard)) => self.virtual_display,
            (Protocol::Vnc, None | Some(Subtype::Wlshare)) | (Protocol::Rdp, _) => false,
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
/// It is also what a phone gets: its own screen is portrait and far too small
/// to be a desktop, so the picker offers it no window to follow
/// (`sizeFollows` in `frontend/src/useRemoteDesktop.ts`). An operator who
/// wants the larger desk configures a `size` and pays for it deliberately, up
/// to the ceiling a video stream encodes within
/// ([`crate::video::MAX_LONG_SIDE`]).
pub const DEFAULT_SIZE: (u16, u16) = (1440, 900);

/// The most virtual displays a target may ask for ([`TargetConfig::virtual_displays`]).
///
/// Two, while the feature is alpha: two is what the picker, the input offset and
/// the span the host builds have been checked with, and each display is a desktop
/// the host renders whether or not anybody is looking at it.
pub const MAX_VIRTUAL_DISPLAYS: u8 = 2;

/// Serde's default for [`TargetConfig::virtual_displays`]: the one desktop every
/// target opens unless asked otherwise.
fn one_display() -> u8 {
    1
}

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
/// reaches its gateway over HTTP and up to four WebSockets, and all need a host
/// and a port. Whatever terminates that proxy is what a browser talks to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddr {
    /// `host:port`, with any IPv6 literal bracketed — resolvable by
    /// [`std::net::ToSocketAddrs`] as it stands.
    Tcp(String),
    /// The path of a Unix socket to create.
    Unix(PathBuf),
    /// A managed worker's named pipe on Windows, where there are no Unix sockets.
    /// Never read from a config: `src/embedded/transport.rs` names it at launch.
    #[cfg(all(feature = "embedded-gateway", windows))]
    Pipe(String),
}

impl std::fmt::Display for ListenAddr {
    /// The way it is written in the config, so a log line can be pasted back into
    /// one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(addr) => f.write_str(addr),
            Self::Unix(path) => write!(f, "{UNIX_LISTEN_PREFIX}{}", path.display()),
            #[cfg(all(feature = "embedded-gateway", windows))]
            Self::Pipe(name) => write!(f, "pipe:{name}"),
        }
    }
}

/// The optional `[server]` block: web-server bind, login, and development host.
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
    /// The `[hevc_wasm]` table: BETA, where the page's software HEVC
    /// decoder is, for a gateway that keeps it outside its data directory. Absent,
    /// the decoder is served if its archive is there. Top-level for
    /// [`Self::branding`]'s reason.
    #[serde(default)]
    pub hevc_wasm: Option<HevcWasmSection>,
    /// The `[hp_decoders]` table: on Windows, the folders a High Performance
    /// target's decoders are loaded from, for a gateway that keeps them off
    /// `PATH`. Absent, they are looked for when a session needs them. Top-level
    /// for [`Self::branding`]'s reason.
    #[serde(default)]
    pub hp_decoders: Option<HpDecoders>,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

/// The `[hp_decoders]` table as written, and as resolved: the folders are
/// absolute, so there is nothing to place. See [`crate::libav`].
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HpDecoders {
    /// The folder holding FFmpeg's `avcodec` and `avutil` DLLs, a shared build's
    /// `bin`. Named, it is the only place FFmpeg is loaded from.
    pub ffmpeg_dir: Option<PathBuf>,
}

impl HpDecoders {
    /// Load each decoder the table names a folder for, before the gateway
    /// listens: a gateway told where a decoder is refuses to start without it, as
    /// with `[hevc_wasm]`'s archive. One it names no folder for is still looked
    /// for when a session needs it.
    pub fn load(&self) -> anyhow::Result<()> {
        // Only Windows is let name a folder, and only a build that loads its
        // decoders: `ConfigFile::parse_with` refuses the table of any other.
        #[cfg(all(windows, not(feature = "apple-hp-media-static")))]
        {
            if let Some(dir) = &self.ffmpeg_dir {
                crate::libav::load_from(dir)?;
            }
        }
        Ok(())
    }
}

/// The `[hevc_wasm]` table as written. See [`crate::hevc_wasm`].
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HevcWasmSection {
    /// The release archive, as downloaded. A relative path is taken from the
    /// gateway's data directory ([`data_dir`]). A gateway told where the archive is
    /// refuses to start without it.
    pub archive: PathBuf,
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
    /// worker uses its private endpoint: a Unix socket in its instance directory, or
    /// a named pipe on Windows.
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
    /// The decoder's release archive, which the gateway reads at start-up: the one
    /// `[hevc_wasm]` names, or the one found in the data directory. `None` serves
    /// no decoder.
    pub hevc_wasm: Option<PathBuf>,
    /// `[hp_decoders]`: the folders the gateway loads the High Performance
    /// decoders from at start-up. Empty looks for them when a session needs them.
    pub hp_decoders: HpDecoders,
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
            // The virtual display is Standard mode's one unofficial extra. High
            // Performance always has one, so the key would say nothing there, and
            // nothing but a Mac has one to open.
            match (target.protocol, target.subtype) {
                _ if !target.virtual_display => {}
                (Protocol::Vnc, Some(Subtype::Ard)) => {}
                (Protocol::Vnc, Some(Subtype::ArdHighPerformance)) => anyhow::bail!(
                    "target {:?} sets virtual_display on an ard-high-performance target, which \
                     always opens a virtual display: the key is subtype \"ard\"'s. Remove it.",
                    target.name
                ),
                (Protocol::Vnc, None | Some(Subtype::Wlshare)) | (Protocol::Rdp, _) => anyhow::bail!(
                    "target {:?} sets virtual_display, which only subtype \"ard\" takes: it \
                     opens Standard Screen Sharing on one of the Mac's virtual displays, \
                     and nothing else here has one to open. Remove the key.",
                    target.name
                ),
            }
        }
        #[cfg(feature = "embedded-gateway")]
        if audience == Audience::Embedded {
            // Refused rather than ignored, and named as a whole block rather than
            // key by key: every one of them is a decision the launcher has already
            // made for this gateway — a private endpoint the control plane proxies
            // to and a token instead of a login. A key that is quietly overridden is
            // worse than one that is refused: it reads as configuration and behaves
            // as decoration.
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
        if let Some(hevc_wasm) = &config.hevc_wasm {
            anyhow::ensure!(
                !hevc_wasm.archive.as_os_str().is_empty(),
                "[hevc_wasm].archive is empty — name the release archive, or leave the table \
                 out for {} in the gateway's data directory",
                crate::hevc_wasm::archive_name()
            );
        }
        if let Some(decoders) = &config.hp_decoders {
            anyhow::ensure!(
                cfg!(windows),
                "[hp_decoders] is for a gateway on Windows, which has no library folder of \
                 its own. Here the decoders are found by the system loader's search. Remove \
                 the table."
            );
            anyhow::ensure!(
                !cfg!(feature = "apple-hp-media-static"),
                "[hp_decoders] names folders to load the decoders from, and this build links \
                 its own. Remove the table."
            );
            let Some(dir) = &decoders.ffmpeg_dir else {
                anyhow::bail!(
                    "[hp_decoders] names no folder — set ffmpeg_dir, or leave the table out \
                     for the decoder found on PATH"
                );
            };
            // Absolute, because a DLL loaded by a relative path is looked for
            // from wherever the gateway happened to be started.
            anyhow::ensure!(
                dir.is_absolute(),
                "[hp_decoders].ffmpeg_dir must be the folder's whole path, drive included"
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
            // A size of nothing is not a size: a zero axis would ask every engine
            // for a desktop that cannot exist.
            anyhow::ensure!(
                target.size.is_none_or(|(w, h)| w > 0 && h > 0),
                "target {:?} sets a {} size, but its width and height must both be \
                 greater than zero",
                target.name,
                size_text(target.size)
            );
            // A configured size is asked for as pixels at 1x, so the one oversize
            // the video stream refuses that check-config *can* see is a size
            // already past the picture ceiling: at runtime the engines hold a
            // screen under it, but holding a configured size would open at one
            // the operator did not choose. (A size under the ceiling at 1x may
            // still land over it on a 2x screen; that one is held, like a screen.)
            anyhow::ensure!(
                target.size.is_none_or(|(w, h)| {
                    crate::video::within_ceiling((u32::from(w), u32::from(h)))
                }),
                "target {:?} sets a {} size, but the video stream encodes at most a \
                 long side of {} and a short side of {} — set a smaller size, or leave \
                 the key out",
                target.name,
                size_text(target.size),
                crate::video::MAX_LONG_SIDE,
                crate::video::MAX_SHORT_SIDE
            );
            // Standard mode shows the Mac's physical displays as they are, so a
            // size there is one no session would ever state.
            anyhow::ensure!(
                target.size.is_none() || target.sized(),
                "target {:?} sets size on subtype \"ard\", which shares the Mac's physical \
                 displays and never sizes them. Remove the key, or set virtual_display = true.",
                target.name
            );
            // A virtual display opens at the client's density under a ceiling of
            // pixels, so a size past the ceiling's points at 2x would be shrunk for
            // a Retina client after the picker had stated it.
            let (most_w, most_h) = crate::vnc_apple::POINTS_AT_ANY_DENSITY;
            anyhow::ensure!(
                !target.has_virtual_display()
                    || target.size.is_none_or(|(w, h)| w <= most_w && h <= most_h),
                "target {:?} sets a {} size, but a Mac's virtual display holds at most \
                 {most_w}x{most_h} on a Retina client, which opens it at 2x — set a size \
                 within that, or leave the key out",
                target.name,
                size_text(target.size)
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
            // H.264 rides the pipeline, so it needs one: an RDP target's, left on.
            anyhow::ensure!(
                !target.egfx_h264 || (target.protocol == Protocol::Rdp && target.egfx()),
                "target {:?} sets egfx_h264, which only an rdp target with its graphics pipeline \
                 on can take: H.264 is drawn on that pipeline. Remove the key{}.",
                target.name,
                if target.protocol == Protocol::Rdp { ", or egfx = false" } else { "" }
            );
            // A count of virtual displays is a count the engine asks the remote
            // for, and two remotes create them: a Windows host and a Mac in High
            // Performance mode. On any other target the key would be read and
            // change nothing.
            anyhow::ensure!(
                (1..=MAX_VIRTUAL_DISPLAYS).contains(&target.virtual_displays),
                "target {:?} sets virtual_displays = {}, which must be 1 to {MAX_VIRTUAL_DISPLAYS}",
                target.name,
                target.virtual_displays
            );
            anyhow::ensure!(
                target.virtual_displays == 1
                    || target.protocol == Protocol::Rdp
                    || target.subtype == Some(Subtype::ArdHighPerformance),
                "target {:?} sets virtual_displays = {}, and only an rdp target or one with \
                 subtype = \"ard-high-performance\" lays out more than one virtual display. \
                 Remove the key.",
                target.name,
                target.virtual_displays
            );
            // The camera rides MS-RDPECAM on RDP and wlshare's camera extension on a
            // `wlshare` target. Neither Apple's Screen Sharing nor a VNC server read
            // as a plain one speaks such an extension.
            let carries_devices = target.protocol == Protocol::Rdp || target.wlshare();
            let kind = target.subtype.map_or("plain vnc", Subtype::name);
            anyhow::ensure!(
                !target.camera || carries_devices,
                "target {:?} is {kind} and sets camera, which has nowhere to go there: the \
                 camera rides MS-RDPECAM on rdp and wlshare's camera extension on a vnc target \
                 with subtype = \"wlshare\". Remove the key.",
                target.name
            );
            // The microphone likewise: MS-RDPEAI on RDP, wlshare's microphone extension on
            // a `wlshare` target, and nothing anywhere else.
            anyhow::ensure!(
                !target.microphone || carries_devices,
                "target {:?} is {kind} and sets microphone, which has nowhere to go there: \
                 the microphone rides MS-RDPEAI on rdp and wlshare's microphone extension on a \
                 vnc target with subtype = \"wlshare\". Remove the key.",
                target.name
            );
            // The bitrate keys and the adaptive switch tune the Opus encoder, for the
            // sessions that take the target's sound: the one here for MS-RDPEA's
            // PCM on RDP, and wlshare's own on a `wlshare` target, which is told
            // the rate over its audio extension ([`crate::vnc_audio`]). On `ard` and on a plain `vnc` target no
            // session has any, and `ard-high-performance`'s is passed as the Mac's
            // own AAC-ELD ([`crate::vnc_apple_media`]), so the keys could not do
            // anything there.
            let sound = target.offers().audio;
            anyhow::ensure!(
                target.audio_bitrate.is_none() || sound,
                "target {:?} is {kind} and sets audio_bitrate — it is the encoder's rate, \
                 and no session there has sound to encode. Remove the key.",
                target.name
            );
            // Either way: `false` without sound is as unreadable as `true`, and a key
            // nothing reads is a mistake to report, not a preference to keep.
            anyhow::ensure!(
                target.audio_adaptive.is_none() || sound,
                "target {:?} is {kind} and sets audio_adaptive — adapting means moving the \
                 encoder's bitrate, and no session there has sound to encode. Remove the key.",
                target.name
            );
            if let Some(kbps) = target.audio_bitrate {
                anyhow::ensure!(
                    (6..=510).contains(&kbps),
                    "target {:?} sets audio_bitrate = {kbps}, which is out of range — it is \
                     in kbit/s and must be 6–510",
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
                }
                (Protocol::Vnc, None | Some(Subtype::Wlshare)) => {
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
            if target.protocol != Protocol::Rdp {
                anyhow::ensure!(
                    target.domain.is_none(),
                    "target {:?} is protocol {:?} but sets domain, which only \"rdp\" targets use",
                    target.name,
                    target.protocol.name()
                );
            }
            if let Some(q) = target.video_quality {
                anyhow::ensure!(
                    (1..=100).contains(&q),
                    "target {:?} sets video_quality = {q}, which is out of range — it \
                     must be 1–100",
                    target.name
                );
            }
        }
        Ok(config)
    }

    /// Resolve the runtime configuration of a managed local instance: its private
    /// endpoint and a freshly minted token.
    ///
    /// Both are arguments here rather than a default that
    /// `[server]` could override, which is what [`Audience::Embedded`] enforces on
    /// the way in. `[branding]` is the one thing such a config *may* say about the
    /// gateway itself: it names the instance, and multiple local instances are
    /// easier to tell apart if they can be called different things.
    ///
    /// `state_dir` is the instance directory, where `[meter]` keeps its database, and
    /// `data_dir` is where `[hevc_wasm]` finds its archive; see [`data_dir`].
    #[cfg(feature = "embedded-gateway")]
    pub fn resolve_embedded(
        self,
        token: EmbeddedToken,
        endpoint: ListenAddr,
        state_dir: &Path,
        data_dir: &Path,
    ) -> anyhow::Result<AppConfig> {
        let branding = Self::resolve_branding(self.branding.as_ref())?;
        Ok(AppConfig {
            // Only the native control plane reaches this listener. It owns the TCP
            // origin a browser addresses and proxies both HTTP and WebSockets here.
            listen: endpoint,
            targets: self.targets,
            auth: GatewayAuth::Token(token),
            branding,
            dev_hostname: None,
            meter: Self::resolve_meter(self.meter, state_dir),
            hevc_wasm: Self::resolve_hevc_wasm(self.hevc_wasm, data_dir),
            hp_decoders: self.hp_decoders.unwrap_or_default(),
        })
    }

    /// Where the software HEVC decoder's archive is: the file `[hevc_wasm]` names,
    /// placed in `data_dir`, or with no table the release's own name there if such a
    /// file exists — `None` if not, which is a gateway nobody gave the decoder to.
    /// Only a path: the archive is read and checked when the gateway starts
    /// ([`crate::hevc_wasm::HevcDecoder::load`]), as `[meter]`'s database is opened
    /// then, so a named archive that is missing, and either one that is not the
    /// pinned release, is a refused start.
    fn resolve_hevc_wasm(section: Option<HevcWasmSection>, data_dir: &Path) -> Option<PathBuf> {
        match section {
            // `join` keeps an absolute path as written.
            Some(section) => Some(data_dir.join(section.archive)),
            None => Some(data_dir.join(crate::hevc_wasm::archive_name())).filter(|archive| archive.is_file()),
        }
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
    /// For checking a config that may not be in any file: the state and data
    /// directories are the working directory, and nothing is opened in either.
    pub fn resolve(self) -> anyhow::Result<AppConfig> {
        self.resolve_with(None, Path::new(""), Path::new(""))
    }

    /// Resolve the runtime configuration: validate the web-login credential and
    /// carry over every target profile (the browser picks one after login).
    ///
    /// `listen` is `--listen`/`REMOTEX_LISTEN` when either was given, and it wins
    /// over `[server].listen`. That is the whole precedence: one address, from the
    /// command line if it is there and from the file otherwise.
    ///
    /// `state_dir` is where `[meter]` keeps its database; see [`state_dir`].
    /// `data_dir` is where `[hevc_wasm]` finds its archive; see [`data_dir`].
    pub fn resolve_with(
        self,
        listen: Option<&str>,
        state_dir: &Path,
        data_dir: &Path,
    ) -> anyhow::Result<AppConfig> {
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
            hevc_wasm: Self::resolve_hevc_wasm(self.hevc_wasm, data_dir),
            hp_decoders: self.hp_decoders.unwrap_or_default(),
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

/// Where the gateway finds the files that come with its version but no build
/// holds — the software HEVC decoder's archive: `share/remotex` in its release
/// tree, beside the `share/doc/remotex` every release target installs. That is
/// `/usr/share/remotex` for the `.deb` and `.rpm`, `/usr/local/share/remotex` for
/// the macOS `.pkg`, `share\remotex` under the `.msi`'s install directory,
/// and `/opt/remotex/versions/<version>/share/remotex` in the container image.
/// They follow the binary, not the config, and are
/// replaced with it: they are pinned to its version, unlike the state directory.
///
/// A binary outside a release tree — a Cargo build — has `outside`: the config's
/// directory, or the embedded instance's.
pub fn data_dir(outside: &Path) -> PathBuf {
    running_exe()
        .and_then(|exe| data_dir_for_exe(&exe))
        .unwrap_or_else(|| outside.to_path_buf())
}

/// Every release target puts the binary in a release tree's `bin`.
fn data_dir_for_exe(exe: &Path) -> Option<PathBuf> {
    let bin_dir = exe.parent()?;
    if !bin_dir.file_name()?.eq_ignore_ascii_case("bin") {
        return None;
    }
    Some(bin_dir.parent()?.join("share").join("remotex"))
}

/// The executable that is actually running, through any link to it: the
/// container's `/opt/remotex/current` is one.
fn running_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.canonicalize().unwrap_or(exe))
}

/// Resolve the package-manager layout or the container image's versioned layout
/// from the executable that is actually running.
fn installed_layout() -> Option<InstalledLayout> {
    installed_layout_for_exe(&running_exe()?)
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

    // By default the Windows package installs the same tree under %ProgramFiles%\remotex:
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
    fn the_data_directory_is_share_remotex_in_each_release_tree() {
        let data = |exe: &str| data_dir_for_exe(Path::new(exe));
        assert_eq!(data("/usr/bin/remotex"), Some("/usr/share/remotex".into()), ".deb and .rpm");
        assert_eq!(data("/usr/local/bin/remotex"), Some("/usr/local/share/remotex".into()), ".pkg");
        assert_eq!(
            data("/opt/remotex/versions/0.0.294/bin/remotex"),
            Some("/opt/remotex/versions/0.0.294/share/remotex".into()),
            "the container image"
        );
        assert_eq!(
            data("/home/me/remotex-0.0.294-linux-x86_64/bin/remotex"),
            Some("/home/me/remotex-0.0.294-linux-x86_64/share/remotex".into()),
            "any other release tree"
        );
        #[cfg(windows)]
        assert_eq!(
            data(r"C:\Program Files\remotex\bin\remotex.exe"),
            Some(PathBuf::from(r"C:\Program Files\remotex\share\remotex")),
            ".msi"
        );
        assert_eq!(data("/checkout/target/debug/remotex"), None, "a Cargo build");
    }

    #[test]
    fn a_config_outside_an_installation_keeps_state_in_its_own_directory() {
        assert_eq!(state_dir(Path::new("/home/me/remotex/uat.toml")), Path::new("/home/me/remotex"));
        assert_eq!(state_dir(Path::new("uat.toml")), Path::new(""), "the working directory");
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
        assert_eq!(t.size, None, "no size is configured");
        assert_eq!(t.kept_size(), DEFAULT_SIZE);
        assert_eq!((t.username.as_str(), t.password.as_str(), t.domain.as_deref()), ("u", "p", None));
        assert!(t.egfx(), "the graphics pipeline is on unless turned off");
        assert!(!t.sound(Choices::default()), "the remote's sound is taken only where it is chosen");
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
            file.clone().resolve_with(Some("0.0.0.0:8080"), Path::new(""), Path::new("")).unwrap().listen.to_string(),
            "0.0.0.0:8080"
        );
        // Absent, the file still decides.
        assert_eq!(
            file.clone().resolve_with(None, Path::new(""), Path::new("")).unwrap().listen.to_string(),
            "127.0.0.1:1"
        );
        // And a config with no address at all falls back to the default.
        assert_eq!(
            ConfigFile::parse(&minimal())
                .unwrap()
                .resolve_with(None, Path::new(""), Path::new(""))
                .unwrap()
                .listen
                .to_string(),
            DEFAULT_LISTEN
        );

        let err = file.resolve_with(Some("0.0.0.0"), Path::new(""), Path::new("")).unwrap_err();
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
            ConfigFile::parse(&toml).unwrap().resolve_with(None, state, Path::new("")).unwrap().meter
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

    /// With no table, the decoder is the release archive found by its release name in
    /// the data directory, not the state directory, and none where there is no such
    /// file. A table names another, there or not: the gateway reads the archive when
    /// it starts, and refuses one it was told of and cannot read.
    #[test]
    fn the_hevc_decoder_is_looked_for_in_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let archive = |table: &str| {
            let toml = format!("{table}\n{}", minimal());
            ConfigFile::parse(&toml)
                .unwrap()
                .resolve_with(None, Path::new("/var/lib/remotex"), data)
                .unwrap()
                .hevc_wasm
        };
        assert_eq!(archive(""), None, "no archive serves no decoder");
        assert_eq!(
            archive("[hevc_wasm]\narchive = \"decoders/hevc.tar.gz\""),
            Some(data.join("decoders/hevc.tar.gz"))
        );
        // A whole path where this runs: on Windows one without a drive is not.
        let elsewhere = if cfg!(windows) { r"C:\opt\hevc.tar.gz" } else { "/opt/hevc.tar.gz" };
        let named = format!("[hevc_wasm]\narchive = '{elsewhere}'");
        assert_eq!(archive(&named), Some(PathBuf::from(elsewhere)));
        let released = data.join(crate::hevc_wasm::archive_name());
        std::fs::write(&released, b"").unwrap();
        assert_eq!(archive(""), Some(released), "the archive is found where the release puts it");
        assert_eq!(archive(&named), Some(PathBuf::from(elsewhere)), "a named archive is the one read");
        for (bad, says) in [
            ("", "archive"),
            ("archive = \"\"", "[hevc_wasm].archive"),
            ("enabled = true\narchive = \"h.tar.gz\"", "enabled"),
            ("archive = \"h.tar.gz\"\ndir = \"/opt/hevc\"", "dir"),
        ] {
            let err = ConfigFile::parse(&format!("[hevc_wasm]\n{bad}\n{}", minimal()))
                .expect_err(bad);
            assert!(format!("{err:#}").contains(says), "{bad}: {err:#}");
        }
    }

    /// `[hp_decoders]` is Windows': a gateway there names the folder its decoder
    /// is loaded from, a whole path. Every other gateway refuses the table, its
    /// loader having a search of its own.
    #[test]
    fn hp_decoders_names_whole_folders_on_windows_alone() {
        let parse = |table: &str| ConfigFile::parse(&format!("{table}\n{}", minimal()));
        assert_eq!(
            parse("").unwrap().resolve().unwrap().hp_decoders,
            HpDecoders::default(),
            "no table names no folder"
        );
        #[cfg(not(windows))]
        {
            let err = parse("[hp_decoders]\nffmpeg_dir = \"/opt/ffmpeg/bin\"").expect_err("not Windows");
            assert!(format!("{err:#}").contains("on Windows"), "{err:#}");
        }
        #[cfg(all(windows, not(feature = "apple-hp-media-static")))]
        {
            assert_eq!(
                parse("[hp_decoders]\nffmpeg_dir = 'C:\\ffmpeg\\bin'").unwrap().resolve().unwrap().hp_decoders,
                HpDecoders { ffmpeg_dir: Some(PathBuf::from(r"C:\ffmpeg\bin")) }
            );
            for (bad, says) in [
                ("", "names no folder"),
                ("ffmpeg_dir = 'ffmpeg\\bin'", "[hp_decoders].ffmpeg_dir"),
                ("ffmpeg_dir = '\\ffmpeg'", "[hp_decoders].ffmpeg_dir"),
                ("fdk_aac_dir = 'C:\\fdk'", "fdk_aac_dir"),
                ("dir = 'C:\\ffmpeg\\bin'", "dir"),
            ] {
                let err = parse(&format!("[hp_decoders]\n{bad}")).expect_err(bad);
                assert!(format!("{err:#}").contains(says), "{bad}: {err:#}");
            }
        }
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
            size = "1920x1080"

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
        assert_eq!(win.size, Some((1920, 1080)));
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

    /// A target with no stream keys streams the whole desktop at the default dial,
    /// with the adaptive walk on and the browser choosing the chroma.
    #[test]
    fn a_bare_target_streams_at_the_defaults() {
        let cfg = parse_target("").expect("a bare target");
        for decoder in [Chroma::Subsampled, Chroma::Full] {
            assert_eq!(
                cfg.targets[0].render_plan(Choices::default(), decoder.into()),
                RenderPlan {
                    quality: DEFAULT_VIDEO_QUALITY,
                    adaptive: true,
                    chroma: decoder,
                    apple_media: false,
                    rdp_graphics: false,
                    rdp_h264: false,
                }
            );
        }
    }

    #[test]
    fn a_video_quality_is_the_streams_dial() {
        let cfg = parse_target("video_quality = 60").expect("a quality");
        assert_eq!(
            cfg.targets[0].render_plan(Choices::default(), Chroma::Subsampled.into()),
            RenderPlan {
                quality: 60,
                adaptive: true,
                chroma: Chroma::Subsampled,
                apple_media: false,
                rdp_graphics: false,
                rdp_h264: false,
            }
        );
    }

    #[test]
    fn a_video_quality_out_of_range_is_rejected() {
        for q in ["0", "101"] {
            let err = parse_target(&format!("video_quality = {q}")).unwrap_err();
            assert!(format!("{err:#}").contains("1–100"), "q={q}: {err:#}");
        }
    }

    /// A target that writes no chroma gets the browser's answer: unset resolves
    /// exactly as `"auto"` does. A target that names a profile is not moved by the
    /// decoder in front of it — that is the whole difference between selecting a
    /// chroma and leaving it to be resolved.
    #[test]
    fn render_chroma_defaults_to_the_browsers_answer() {
        let video = |extra: &str, decoder: Chroma| {
            parse_target(&format!("video_quality = 100\n{extra}")).unwrap().targets[0]
                .render_plan(Choices::default(), decoder.into())
        };
        let stream = |chroma| RenderPlan {
            quality: 100,
            adaptive: true,
            chroma,
            apple_media: false,
            rdp_graphics: false,
            rdp_h264: false,
        };
        assert_eq!(video("", Chroma::Full), stream(Chroma::Full));
        assert_eq!(video("", Chroma::Subsampled), stream(Chroma::Subsampled));
        assert_eq!(video("render_chroma = \"auto\"", Chroma::Full), stream(Chroma::Full));
        assert_eq!(video("render_chroma = \"auto\"", Chroma::Subsampled), stream(Chroma::Subsampled));
        // A named profile asks nobody.
        assert_eq!(video("render_chroma = \"420\"", Chroma::Full), stream(Chroma::Subsampled));
        assert_eq!(video("render_chroma = \"444\"", Chroma::Subsampled), stream(Chroma::Full));
        // And the key takes only the two samplings VP9 profiles 0 and 1 are.
        let err = parse_target("render_chroma = \"422\"").unwrap_err();
        assert!(format!("{err:#}").contains("422"), "{err:#}");
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
            parse_target(&format!("video_quality = 60\n{extra}")).unwrap().targets[0].render_summary()
        };
        assert_eq!(summary(""), "video q60 chroma auto · adaptive");
        assert_eq!(summary("render_chroma = \"auto\""), summary(""));
        assert_eq!(summary("render_chroma = \"420\""), "video q60 4:2:0 · adaptive");
        assert_eq!(summary("render_chroma = \"444\""), "video q60 4:4:4 · adaptive");
        assert_eq!(summary("render_adaptive = false"), "video q60 chroma auto");
    }

    /// The session card names the resolved plan: the dial, the chroma on the wire,
    /// and the floor where the walk runs.
    #[test]
    fn a_session_card_describes_the_resolved_stream() {
        let describe = |keys: &str, decoder: Chroma| {
            parse_target(keys).unwrap().targets[0].render_plan(Choices::default(), decoder.into()).describe()
        };
        assert_eq!(describe("video_quality = 60", Chroma::Subsampled), "video q60 4:2:0 · adaptive");
        assert_eq!(describe("video_quality = 60", Chroma::Full), "video q60 4:4:4 · adaptive");
        assert_eq!(
            describe("video_quality = 60\nrender_chroma = \"444\"", Chroma::Subsampled),
            "video q60 4:4:4 · adaptive"
        );
        assert_eq!(
            describe("video_quality = 60\nrender_adaptive = false", Chroma::Subsampled),
            "video q60 4:2:0"
        );
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
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(config.targets[0].protocol, Protocol::Vnc);
        assert_eq!(config.targets[0].port, 5900);

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

    /// The Apple subtypes.
    const APPLE_SUBTYPES: &[&str] = &["ard", "ard-high-performance"];

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

        // Standard mode exposes physical displays, which this gateway never resizes,
        // and never touches the Mac's sound: the picker has nothing to offer there.
        let standard = &ard("username = \"andrew\"\npassword = \"h\"").unwrap().targets[0];
        assert_eq!(standard.offers(), Offers { resize: false, audio: false, passthrough: None, placement: false });

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

    /// The unofficial `virtual_display` opens Standard mode on a virtual display:
    /// it is `ard`'s key alone, and the one thing that lets `ard` offer resize.
    /// A build without the media stream's decoders takes it, which is its point.
    #[test]
    fn ard_opens_a_virtual_display_only_when_asked() {
        let ard = |extra: &str| {
            ConfigFile::parse(&vnc_toml(&format!(
                "subtype = \"ard\"\nusername = \"andrew\"\npassword = \"h\"\n{extra}"
            )))
        };

        let plain = &ard("").unwrap().targets[0];
        assert!(!plain.virtual_display);
        assert!(!plain.has_virtual_display());
        assert!(!plain.media_stream(), "no stream, and no sound, on either");

        let target = &ard("virtual_display = true\nsize = \"1600x1000\"")
            .unwrap()
            .targets[0];
        assert_eq!(target.subtype, Some(Subtype::Ard), "still Standard mode");
        assert!(target.has_virtual_display());
        assert_eq!(
            target.offers(),
            Offers { resize: true, audio: false, passthrough: None, placement: false },
            "a display to resize, and no sound on Standard's virtual display either"
        );
        assert!(!target.media_stream());
        assert!(!target.offers().audio);
        assert_eq!(target.size, Some((1600, 1000)));
        // At a kept size, the display opens at it and stays there, as High
        // Performance does.
        assert!(ard("virtual_display = true").unwrap().targets[0].has_virtual_display());

        // The key says nothing on High Performance, and nothing else has a virtual
        // display to open.
        let refused = [
            ("subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"\n", "always opens a virtual display"),
            ("", "only subtype \"ard\" takes"),
        ];
        for (subtype, reason) in refused {
            let err = ConfigFile::parse(&vnc_toml(&format!("{subtype}virtual_display = true")))
                .unwrap_err();
            assert!(format!("{err:#}").contains(reason), "{err:#}");
        }
        let err = ConfigFile::parse(&format!(
            "[server]\n{}\n\n[[targets]]\nname = \"pc\"\nprotocol = \"rdp\"\nhost = \"10.0.0.5\"\n\
             username = \"Administrator\"\npassword = \"h\"\nvirtual_display = true\n",
            site_passwd_line()
        ))
        .unwrap_err();
        assert!(format!("{err:#}").contains("only subtype \"ard\" takes"), "{err:#}");
    }

    /// The high-performance subtype carries the same account credentials as plain
    /// `ard`, and requests a virtual display at the configured size.
    #[test]
    fn the_high_performance_subtype_offers_resize() {
        let hp = |extra: &str| {
            ConfigFile::parse(&vnc_toml(&format!(
                "subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"\n{extra}"
            )))
        };

        let target = &hp("size = \"1600x1000\"").unwrap().targets[0];
        assert_eq!(target.subtype, Some(Subtype::ArdHighPerformance));
        assert_eq!(target.size, Some((1600, 1000)));
        assert!(target.offers().resize);
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

    /// The Mac's stream is passed in a session started with the passthrough and in
    /// no other, whatever the browser decodes: a browser that cannot take it is not
    /// served that session, rather than sent another. It is the media stream's
    /// choice, offered by no target without one.
    #[test]
    fn a_high_performance_session_passes_the_macs_stream_where_it_was_chosen() {
        let mac = |subtype: &str| {
            ConfigFile::parse(&vnc_toml(&format!(
                "subtype = \"{subtype}\"\nusername = \"andrew\"\npassword = \"h\"\n"
            )))
            .unwrap()
            .targets
            .remove(0)
        };
        let hp = mac("ard-high-performance");
        let takes = Decoders { chroma: Chroma::Full, apple_media: true, rdp_graphics: false, rdp_h264: false };
        let declines = Decoders { chroma: Chroma::Full, apple_media: false, rdp_graphics: false, rdp_h264: false };
        let passed = Choices { passthrough: true, ..Choices::default() };

        assert_eq!(hp.offers().passthrough, Some(Passthrough::AppleMedia));
        assert!(hp.render_plan(passed, takes).apple_media);
        assert_eq!(
            hp.render_plan(passed, takes).describe(),
            "the Mac's HEVC, passed through"
        );
        assert!(!hp.render_plan(Choices::default(), takes).apple_media, "only the choice passes it");
        assert_eq!(hp.beyond(passed, takes), None);
        assert_eq!(hp.beyond(passed, declines), Some(Passthrough::AppleMedia));
        assert_eq!(hp.beyond(Choices::default(), declines), None, "VP9 is for every browser");
        assert_eq!(hp.render_summary(), "video q90 chroma auto · adaptive", "the file chooses none");

        // The sound comes with the picture, chosen or not, so it is not offered.
        assert!(!hp.offers().audio);
        assert!(hp.sound(Choices::default()));
        assert_eq!(
            hp.accepts(Choices { audio: Sound::Opus, ..Choices::default() }),
            Err(NotOffered { target: "mac".to_owned(), choice: "sound" })
        );

        let standard = mac("ard");
        assert_eq!(standard.offers().passthrough, None);
        assert_eq!(
            standard.accepts(passed),
            Err(NotOffered { target: "mac".to_owned(), choice: "a passthrough" })
        );
        assert_eq!(
            standard.accepts(Choices { size: Sizing::Window, ..Choices::default() }),
            Err(NotOffered { target: "mac".to_owned(), choice: "resize" })
        );

        // The four keys these choices replaced are no longer keys.
        for key in ["resize = true", "audio = true", "media_passthrough = true", "egfx_passthrough = true"] {
            let err = ConfigFile::parse(&vnc_toml(key)).unwrap_err();
            assert!(format!("{err:#}").contains("unknown field"), "{key}: {err:#}");
        }
    }

    /// The opening size resolves the same way for every engine: a kept size is
    /// the configured one or the default, and only a session that follows the
    /// window opens at the client's screen.
    #[test]
    fn the_opening_size_is_the_kept_size_or_the_clients_screen() {
        let screen = HostDisplay { w: 1728, h: 1117, scale: 200, fit: false };
        let phone = HostDisplay { w: 430, h: 932, scale: 300, fit: true };

        let sized = &ConfigFile::parse(&vnc_toml("size = \"1600x1000\"")).unwrap().targets[0];
        assert_eq!(sized.kept_size(), (1600, 1000));
        let unsized_ = &ConfigFile::parse(&vnc_toml("")).unwrap().targets[0];
        assert_eq!(unsized_.kept_size(), DEFAULT_SIZE);

        for display in [Some(screen), Some(phone), None] {
            // The target's size, whatever screen the client has.
            assert_eq!(sized.opening_size(Sizing::Target, display), (1600, 1000));
            assert_eq!(unsized_.opening_size(Sizing::Target, display), DEFAULT_SIZE);
            // The default, though the target configures another.
            assert_eq!(sized.opening_size(Sizing::BuiltIn, display), DEFAULT_SIZE);
        }

        // Following the window, the configured size is not used: the session opens
        // at the client's screen. A pinch-zoom client's screen is not an opening
        // size, and neither is no screen at all.
        for target in [sized, unsized_] {
            assert_eq!(target.opening_size(Sizing::Window, Some(screen)), (1728, 1117));
            assert_eq!(target.opening_size(Sizing::Window, Some(phone)), DEFAULT_SIZE);
            assert_eq!(target.opening_size(Sizing::Window, None), DEFAULT_SIZE);
        }
    }

    /// One key, a width by a height. The two keys it replaced are no longer keys.
    #[test]
    fn a_size_is_written_as_a_width_by_a_height() {
        for wrong in ["1600", "1600x", "x1000", "1600 x 1000", "1600X1000", "1600x1000x2", "70000x1000"] {
            let err = ConfigFile::parse(&vnc_toml(&format!("size = \"{wrong}\""))).unwrap_err();
            assert!(format!("{err:#}").contains("is not a width by a height"), "{wrong}: {err:#}");
        }
        for key in ["width = 1600", "height = 1000", "size = 1600"] {
            ConfigFile::parse(&vnc_toml(key)).expect_err(key);
        }
    }

    /// A virtual display opens at the client's density, so the picker's size is
    /// one a 2x client's display can hold.
    #[test]
    fn a_size_a_retina_client_would_shrink_is_refused_on_a_virtual_display() {
        let hp = "subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"\n";
        let virt = "subtype = \"ard\"\nvirtual_display = true\nusername = \"andrew\"\npassword = \"h\"\n";
        for mac in [hp, virt] {
            ConfigFile::parse(&vnc_toml(&format!("{mac}size = \"1920x1080\""))).expect("the most it holds at 2x");
            for over in ["2560x1440", "1921x1080", "1920x1200"] {
                let err = ConfigFile::parse(&vnc_toml(&format!("{mac}size = \"{over}\""))).unwrap_err();
                assert!(format!("{err:#}").contains("at most 1920x1080"), "{err:#}");
            }
        }
        ConfigFile::parse(&vnc_toml("size = \"2560x1440\"")).expect("no ceiling of points elsewhere");
    }

    /// Standard mode shows the Mac's physical displays as they are, so a size there
    /// would never be stated.
    #[test]
    fn a_size_is_refused_on_a_mac_sharing_its_physical_displays() {
        let ard = "subtype = \"ard\"\nusername = \"andrew\"\npassword = \"h\"\n";
        let standard = &ConfigFile::parse(&vnc_toml(ard)).unwrap().targets[0];
        assert!(!standard.sized());
        let err = ConfigFile::parse(&vnc_toml(&format!("{ard}size = \"1600x1000\""))).unwrap_err();
        assert!(format!("{err:#}").contains("never sizes them"), "{err:#}");
        let virtual_display =
            &ConfigFile::parse(&vnc_toml(&format!("{ard}virtual_display = true\nsize = \"1600x1000\"")))
                .unwrap()
                .targets[0];
        assert!(virtual_display.sized());
    }

    /// A count of virtual displays is one unless asked, at most two, and a key
    /// only an rdp target and a High Performance Mac take: everywhere else it
    /// would change nothing. The pipeline's passthrough keeps its row beside two:
    /// the browser composes the span once and shows a display of it.
    #[test]
    fn virtual_displays_are_one_unless_asked_and_only_where_they_are_created() {
        let one = ConfigFile::parse(&rdp_toml("")).unwrap().targets.remove(0);
        assert_eq!(one.virtual_displays, 1);
        assert_eq!(one.offers().passthrough, Some(Passthrough::RdpGraphics));

        let two = ConfigFile::parse(&rdp_toml("virtual_displays = 2")).unwrap().targets.remove(0);
        assert_eq!(two.virtual_displays, 2);
        assert_eq!(two.offers().passthrough, Some(Passthrough::RdpGraphics), "the passthrough shows one display of the span");
        assert!(two.offers().resize, "the window still drives each display's size");
        assert_eq!(two.accepts(Choices { passthrough: true, ..Choices::default() }), Ok(()));
        // Where the second sits is chosen only where there is a second, on a host
        // that is told where: the Mac places its own.
        let below = Choices { placement: Placement::Bottom, ..Choices::default() };
        assert_eq!(two.accepts(below), Ok(()));
        assert_eq!(one.accepts(below).unwrap_err().choice, "a place for the second display");

        for bad in ["virtual_displays = 0", "virtual_displays = 3"] {
            let err = ConfigFile::parse(&rdp_toml(bad)).unwrap_err();
            assert!(format!("{err:#}").contains("must be 1 to 2"), "{bad}: {err:#}");
        }
        let err = ConfigFile::parse(&vnc_toml("virtual_displays = 2
vnc_password = \"x\"")).unwrap_err();
        assert!(format!("{err:#}").contains("lays out more than one virtual display"), "{err:#}");
        // Standard mode's unofficial virtual display is one.
        let err = ConfigFile::parse(&vnc_toml(
            "subtype = \"ard\"\nusername = \"andrew\"\npassword = \"h\"\nvirtual_display = true\nvirtual_displays = 2",
        ))
        .unwrap_err();
        assert!(format!("{err:#}").contains("lays out more than one virtual display"), "{err:#}");
        let mac = ConfigFile::parse(&vnc_toml(
            "subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"\nvirtual_displays = 2",
        ))
        .unwrap()
        .targets
        .remove(0);
        assert_eq!(mac.virtual_displays, 2);
        assert!(mac.accepts(below).is_err());
        assert_eq!(mac.offers().passthrough, Some(Passthrough::AppleMedia), "each display is a stream of its own");
        // One is every target's default and so is accepted anywhere.
        ConfigFile::parse(&vnc_toml("virtual_displays = 1
vnc_password = \"x\"")).unwrap();
    }

    /// Which sizings a target takes: following a window where the window can drive
    /// it, and the default beside a configured size only there.
    #[test]
    fn a_sizing_is_refused_where_the_target_does_not_offer_it() {
        let window = Choices { size: Sizing::Window, ..Choices::default() };
        let built_in = Choices { size: Sizing::BuiltIn, ..Choices::default() };
        let refused = |choice| Err(NotOffered { target: "mac".to_owned(), choice });

        // A plain target is asked for a size once and never follows a window.
        let plain = &ConfigFile::parse(&vnc_toml("size = \"1600x1000\"")).unwrap().targets[0];
        assert!(!plain.offers().resize);
        assert_eq!(plain.accepts(Choices::default()), Ok(()));
        assert_eq!(plain.accepts(window), refused("resize"));
        assert_eq!(plain.accepts(built_in), refused("the default size"));

        let wlshare = |extra: &str| {
            ConfigFile::parse(&vnc_toml(&format!("subtype = \"wlshare\"\n{extra}"))).unwrap().targets.remove(0)
        };
        let sized = wlshare("size = \"1600x1000\"");
        for choices in [Choices::default(), window, built_in] {
            assert_eq!(sized.accepts(choices), Ok(()));
        }
        // With no size configured, the target's size already is the default.
        let unsized_ = wlshare("");
        assert_eq!(unsized_.accepts(window), Ok(()));
        assert_eq!(unsized_.accepts(built_in), refused("the default size"));
    }

    /// The one oversize check-config can see: a size the video stream would refuse
    /// at 1x.
    #[test]
    fn a_size_over_the_video_ceiling_is_refused() {
        let err = ConfigFile::parse(&rdp_toml("size = \"5120x2880\""))
            .expect_err("a 5K size parsed");
        assert!(format!("{err:#}").contains("3840"), "{err:#}");
        for size in ["size = \"3840x2400\"", "size = \"2400x3840\""] {
            ConfigFile::parse(&rdp_toml(size))
                .expect("a 4K size, either way up, is a picture the stream takes");
        }
    }

    /// A zero axis is refused on every target that takes a size alike — a High
    /// Performance virtual display was merely the first place it was caught
    /// misbehaving.
    #[test]
    fn a_size_requires_nonzero_dimensions() {
        for size in ["size = \"0x1000\"", "size = \"1600x0\""] {
            let apple = APPLE_SUBTYPES.iter().map(|subtype| {
                format!("subtype = \"{subtype}\"\nusername = \"andrew\"\npassword = \"h\"\n")
            });
            for subtype in std::iter::once(String::new()).chain(apple) {
                let err = ConfigFile::parse(&vnc_toml(&format!("{subtype}{size}")))
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

    #[test]
    fn a_domain_on_a_vnc_target_is_rejected() {
        let apple = APPLE_SUBTYPES.iter().map(|subtype| format!("subtype = \"{subtype}\"\n"));
        for subtype in std::iter::once(String::new()).chain(apple) {
            let err = ConfigFile::parse(&vnc_toml(&format!(
                "{subtype}username = \"u\"\npassword = \"p\"\ndomain = \"CORP\""
            )))
            .unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("sets domain"), "{subtype:?}: {msg}");
        }
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

    /// The clipboard is every engine's and always bridged, so a target has no key
    /// for it.
    #[test]
    fn clipboard_is_not_a_target_key() {
        for (protocol, host) in [("vnc", "10.0.0.4"), ("rdp", "10.0.0.5")] {
            let err = ConfigFile::parse(&format!(
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
            .unwrap_err();
            assert!(format!("{err:#}").contains("clipboard"), "{protocol}: {err:#}");
        }
    }

    /// RDP and wlshare both offer the remote's sound as a choice; `ard` carries
    /// none and `ard-high-performance` always carries its own, so neither Apple
    /// subtype offers it, and neither does a plain `vnc` target, which lists no
    /// extension to carry any.
    #[test]
    fn sound_is_offered_by_rdp_and_wlshare() {
        let target = |body: &str| {
            ConfigFile::parse(&format!(
                "[server]\n{}\n[[targets]]\nname = \"desk\"\nhost = \"10.0.0.5\"\n{body}\n",
                site_passwd_line()
            ))
            .unwrap()
            .resolve()
            .unwrap()
            .targets
            .remove(0)
        };
        let sound = Choices { audio: Sound::Opus, ..Choices::default() };

        // A plain `vnc` target lists no audio extension.
        let plain = target("protocol = \"vnc\"");
        assert!(!plain.offers().audio);
        assert!(!plain.offers().audio);
        assert_eq!(
            plain.accepts(sound),
            Err(NotOffered { target: "desk".to_owned(), choice: "sound" })
        );

        // A `wlshare` target lists wlshare's audio extension where it was chosen.
        let wlshare = target("protocol = \"vnc\"\nsubtype = \"wlshare\"");
        assert!(wlshare.offers().audio);
        assert_eq!(wlshare.accepts(sound), Ok(()));
        assert!(wlshare.sound(sound));
        assert!(!wlshare.sound(Choices::default()), "a session started without it asks for none");
        assert_eq!(wlshare.audio_source_format(), crate::vnc_audio::SOURCE_FORMAT);

        // An rdp target negotiates MS-RDPEA when it connects, and what the host
        // redirects is CD-quality PCM, which is the source format the encoder is
        // built from.
        let rdp = target("protocol = \"rdp\"\nusername = \"u\"\npassword = \"p\"");
        assert!(rdp.offers().audio);
        assert!(rdp.sound(sound));
        assert!(!rdp.sound(Choices::default()));
        assert_eq!(rdp.audio_source_format(), crate::audio::PCM_CD_QUALITY);
    }

    /// The camera rides MS-RDPECAM on RDP and wlshare's camera extension on a
    /// `wlshare` target, so the key is accepted on both — opt-in (default off) on
    /// each — and refused on both Apple subtypes and on a plain `vnc` target,
    /// none of which speaks such an extension.
    #[test]
    fn camera_rides_rdp_and_wlshare_and_is_refused_elsewhere() {
        let err = ConfigFile::parse(&format!(
            "[server]\n{}\n[[targets]]\nname = \"desk\"\nprotocol = \"vnc\"\nhost = \"10.0.0.7\"\ncamera = true\n",
            site_passwd_line()
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("is plain vnc and sets camera"), "{rendered}");
        assert!(rendered.contains("subtype = \"wlshare\""), "{rendered}");

        for subtype in APPLE_SUBTYPES {
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
            assert!(rendered.contains(&format!("is {subtype} and sets camera")), "{rendered}");
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
            subtype = "wlshare"
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
        assert!(config.targets[1].camera, "a wlshare target lists the extension");
        assert!(!config.targets[2].camera, "the camera is opt-in");
    }

    /// The microphone rides MS-RDPEAI on RDP and wlshare's microphone extension on a
    /// `wlshare` target, and is refused on both Apple subtypes and on a plain `vnc`
    /// target. On either it stands on its own: a remote records with or without
    /// redirected sound.
    #[test]
    fn microphone_rides_rdp_and_wlshare_and_is_refused_elsewhere() {
        let parse = |target: &str| {
            ConfigFile::parse(&format!("[server]\n{}\n\n[[targets]]\n{target}", site_passwd_line()))
                .and_then(|file| file.resolve())
        };
        for subtype in APPLE_SUBTYPES {
            let mac = parse(&format!(
                "name = \"mac\"\nprotocol = \"vnc\"\nsubtype = \"{subtype}\"\nhost = \"10.0.0.5\"\nusername = \"andrew\"\npassword = \"h\"\nmicrophone = true"
            ))
            .unwrap_err();
            let rendered = format!("{mac:#}");
            assert!(rendered.contains(&format!("is {subtype} and sets microphone")), "{rendered}");
            assert!(rendered.contains("wlshare's microphone extension"), "{rendered}");
        }
        let plain = parse("name = \"desk\"\nprotocol = \"vnc\"\nhost = \"10.0.0.7\"\nmicrophone = true").unwrap_err();
        let rendered = format!("{plain:#}");
        assert!(rendered.contains("is plain vnc and sets microphone"), "{rendered}");
        let vnc = parse(
            "name = \"desk\"\nprotocol = \"vnc\"\nsubtype = \"wlshare\"\nhost = \"10.0.0.7\"\nmicrophone = true",
        )
        .unwrap();
        assert!(vnc.targets[0].microphone, "a wlshare target lists the extension");
        let config = parse(
            "name = \"win\"\nprotocol = \"rdp\"\nusername = \"u\"\npassword = \"p\"\nhost = \"10.0.0.5\"\nmicrophone = true",
        )
        .unwrap();
        assert!(config.targets[0].microphone);
        assert!(
            !config.targets[0].sound(Choices::default()),
            "the microphone does not need the remote's sound"
        );
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

    /// H.264 is drawn on the graphics pipeline, so the key is refused where there
    /// is none: on a VNC target, and on an RDP target with the pipeline off.
    #[test]
    fn egfx_h264_needs_a_pipeline_to_ride() {
        let parse = |target: &str| {
            ConfigFile::parse(&format!(
                r#"
                [server]
                {}

                [[targets]]
                name = "nope"
                host = "10.0.0.5"
                {target}
                egfx_h264 = true
                "#,
                site_passwd_line()
            ))
        };
        let vnc = format!("{:#}", parse("protocol = \"vnc\"").unwrap_err());
        assert!(vnc.contains("egfx_h264") && vnc.contains("rdp"), "{vnc}");
        let rdp = "protocol = \"rdp\"\nusername = \"u\"\npassword = \"p\"";
        let bitmap = format!("{:#}", parse(&format!("{rdp}\negfx = false")).unwrap_err());
        assert!(bitmap.contains("egfx_h264") && bitmap.contains("egfx = false"), "{bitmap}");
        assert!(parse(rdp).unwrap().targets[0].egfx_h264);
    }

    /// The pipeline is passed in a session started with the passthrough, to a page
    /// that composes it, and offered only by a target with a pipeline to pass. So
    /// is an RDP resize, which is a graphics reset: the bitmap path offers neither.
    #[test]
    fn an_rdp_target_offers_its_pipeline_and_resize_while_the_pipeline_is_on() {
        let rdp = |extra: &str| {
            ConfigFile::parse(&format!(
                r#"
                [server]
                {}

                [[targets]]
                name = "win"
                protocol = "rdp"
                username = "u"
                password = "p"
                host = "10.0.0.5"
                {extra}
                "#,
                site_passwd_line()
            ))
            .and_then(ConfigFile::resolve)
            .unwrap()
            .targets
            .remove(0)
        };
        let passed = Choices { passthrough: true, ..Choices::default() };
        let win = rdp("");
        assert_eq!(
            win.offers(),
            Offers { resize: true, audio: true, passthrough: Some(Passthrough::RdpGraphics), placement: false }
        );
        for chroma in [Chroma::Subsampled, Chroma::Full] {
            let composes = Decoders { chroma, apple_media: false, rdp_graphics: true, rdp_h264: false };
            let plan = win.render_plan(passed, composes);
            assert!(plan.rdp_graphics);
            assert_eq!(plan.describe(), "the host's graphics pipeline, passed through");
            assert_eq!(win.beyond(passed, composes), None);
            let cannot = Decoders { rdp_graphics: false, rdp_h264: false, ..composes };
            assert_eq!(win.beyond(passed, cannot), Some(Passthrough::RdpGraphics));
            assert!(!win.render_plan(Choices::default(), composes).rdp_graphics);
        }
        assert_eq!(win.render_summary(), "video q90 chroma auto · adaptive");

        // H.264 on the passed pipeline is the target's key, the session's choice
        // and the browser's answer together, and none of them alone.
        let h264 = rdp("egfx_h264 = true");
        let decodes = Decoders { chroma: Chroma::Full, apple_media: false, rdp_graphics: true, rdp_h264: true };
        let plan = h264.render_plan(passed, decodes);
        assert!(plan.rdp_graphics && plan.rdp_h264);
        assert_eq!(plan.describe(), "the host's graphics pipeline with H.264, passed through");
        assert_eq!(h264.offers(), win.offers(), "the key adds no choice to the picker");
        let cannot = Decoders { rdp_h264: false, ..decodes };
        let lossless = h264.render_plan(passed, cannot);
        assert!(lossless.rdp_graphics && !lossless.rdp_h264, "a browser that cannot is passed a pipeline without it");
        assert_eq!(h264.beyond(passed, cannot), None, "and is not turned away");
        assert!(!h264.render_plan(Choices::default(), decodes).rdp_h264, "a composed session never takes it");
        assert!(!win.render_plan(passed, decodes).rdp_h264, "nor a target without the key");

        let bitmap = rdp("egfx = false");
        assert_eq!(bitmap.offers(), Offers { resize: false, audio: true, passthrough: None, placement: false });
        assert_eq!(
            bitmap.accepts(passed),
            Err(NotOffered { target: "win".to_owned(), choice: "a passthrough" })
        );
        assert_eq!(
            bitmap.accepts(Choices { size: Sizing::Window, ..Choices::default() }),
            Err(NotOffered { target: "win".to_owned(), choice: "resize" })
        );
    }

    /// Standard mode never touches the Mac's sound, so an `ard` target has none to
    /// offer and no use for the keys that tune it.
    #[test]
    fn a_standard_mac_carries_no_sound() {
        let target = "[[targets]]\nname = \"mac\"\nprotocol = \"vnc\"\nsubtype = \"ard\"\n\
                      host = \"10.0.0.5\"\nusername = \"andrew\"\npassword = \"h\"\n";
        let config = ConfigFile::parse(&format!("[server]\n{}\n{target}", site_passwd_line()))
            .unwrap()
            .resolve()
            .unwrap();
        assert!(!config.targets[0].offers().audio);
        assert!(!config.targets[0].sound(Choices { audio: Sound::Opus, ..Choices::default() }));

        for key in ["audio_bitrate = 96", "audio_adaptive = false"] {
            let err = ConfigFile::parse(&format!("[server]\n{}\n{target}{key}\n", site_passwd_line()))
                .unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains(key.split(' ').next().unwrap()), "{rendered}");
        }
    }

    /// High Performance brings the Mac's sound on its own media stream, beside the
    /// picture: always on, and not a session's to choose.
    #[test]
    fn high_performance_carries_its_media_streams_sound() {
        let target = "[[targets]]\nname = \"mac\"\nprotocol = \"vnc\"\nsubtype = \"ard-high-performance\"\n\
                      host = \"10.0.0.5\"\nusername = \"andrew\"\npassword = \"h\"\n";
        let config = ConfigFile::parse(&format!("[server]\n{}\n{target}", site_passwd_line()))
            .unwrap()
            .resolve()
            .unwrap();
        let mac = &config.targets[0];
        assert_eq!(mac.subtype, Some(Subtype::ArdHighPerformance));
        assert!(mac.media_stream());
        assert!(mac.sound(Choices::default()), "the sound leg comes with the picture");
        assert!(!mac.offers().audio, "so there is nothing to choose");

        // The sound is the Mac's own AAC-ELD, so the Opus encoder's keys have
        // nothing to tune.
        for key in ["audio_bitrate = 128", "audio_adaptive = false"] {
            let err = ConfigFile::parse(&format!("[server]\n{}\n{target}{key}\n", site_passwd_line()))
                .unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains(key.split(' ').next().unwrap()), "{rendered}");
        }
    }

    /// The pre-negotiation format follows the engine: CD quality is what RDP is
    /// asked for, 48 kHz stereo is what wlshare is asked for.
    #[test]
    fn the_audio_source_format_is_the_engines() {
        // The format is the protocol's, and is what the RDP client asks a host for.
        let rdp = ConfigFile::parse(&format!(
            "[server]\n{}\n[[targets]]\nname = \"w\"\nprotocol = \"rdp\"\nhost = \"h\"\nusername = \"u\"\npassword = \"p\"\n",
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(rdp.targets[0].audio_source_format(), crate::audio::PCM_CD_QUALITY);

        // A wlshare target's is the format this client asks the extension
        // for, which is the same 48 kHz stereo and needs no resampling either.
        let vnc = ConfigFile::parse(&format!(
            "[server]\n{}\n[[targets]]\nname = \"v\"\nprotocol = \"vnc\"\nsubtype = \"wlshare\"\nhost = \"h\"\n",
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(vnc.targets[0].audio_source_format(), crate::vnc_audio::SOURCE_FORMAT);
        assert_eq!(crate::vnc_audio::SOURCE_FORMAT.sample_rate, 48_000);
        assert_eq!(crate::vnc_audio::SOURCE_FORMAT.bits_per_sample, 16);
    }

    // ---- the adaptive dials --------------------------------------------------

    /// One valid target body per test below, parameterized by the keys under test,
    /// on a target whose sessions can carry sound.
    fn parse_audio_target(body: &str) -> anyhow::Result<AppConfig> {
        ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "t"
            protocol = "vnc"
            subtype = "wlshare"
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

    /// Lossless sound is a session's choice, on the two targets whose sound is
    /// one, and not a key of the file.
    #[test]
    fn lossless_sound_is_chosen_at_the_picker_where_there_is_sound_to_choose() {
        let wlshare = parse_audio_target("").unwrap().targets[0].clone();
        let rdp = parse_target("").unwrap().targets[0].clone();
        let chose = |audio| Choices { audio, ..Choices::default() };

        for target in [&wlshare, &rdp] {
            for audio in [Sound::Off, Sound::Opus] {
                assert!(!target.lossless(chose(audio)));
            }
            assert_eq!(target.accepts(chose(Sound::Flac)), Ok(()));
            assert!(target.sound(chose(Sound::Flac)) && target.lossless(chose(Sound::Flac)));
        }

        assert!(parse_target("audio_format = \"flac\"").is_err(), "not a key of the file");
        assert!(serde_json::from_str::<Sound>("\"pcm\"").is_err(), "no third format");
        // No sound to choose a format for: a plain vnc target, and both Macs.
        for kind in ["", "subtype = \"ard\"\nusername = \"u\"", "subtype = \"ard-high-performance\"\nusername = \"u\""] {
            let target = ConfigFile::parse(&format!(
                "[server]\n{}\n[[targets]]\nname = \"v\"\nprotocol = \"vnc\"\n{kind}\nhost = \"h\"\npassword = \"p\"\n",
                site_passwd_line()
            ))
            .and_then(ConfigFile::resolve)
            .unwrap()
            .targets
            .remove(0);
            assert_eq!(
                target.accepts(chose(Sound::Flac)),
                Err(NotOffered { target: "v".to_owned(), choice: "sound" }),
                "{kind}"
            );
            assert!(!target.lossless(chose(Sound::Flac)), "{kind}");
        }
    }

    /// The switch resolves into the plan, and the plan says so.
    #[test]
    fn render_adaptive_resolves_into_the_plan() {
        let cfg = parse_target("video_quality = 80\nrender_adaptive = true").expect("adaptive video");
        let plan = cfg.targets[0].render_plan(Choices::default(), Chroma::Subsampled.into());
        assert_eq!(plan, RenderPlan { quality: 80, adaptive: true, chroma: Chroma::Subsampled, apple_media: false, rdp_graphics: false, rdp_h264: false });
        assert_eq!(plan.describe(), "video q80 4:2:0 · adaptive");
    }

    /// A target that turned the walk off stays exactly on its dial: the
    /// pressure-only walk the stream had before the key existed, and a card that
    /// promises no walk.
    #[test]
    fn render_adaptive_false_leaves_the_plan_without_a_walk() {
        let cfg = parse_target("video_quality = 80\nrender_adaptive = false")
            .expect("video with the walk off");
        let plan = cfg.targets[0].render_plan(Choices::default(), Chroma::Subsampled.into());
        assert_eq!(plan, RenderPlan { quality: 80, adaptive: false, chroma: Chroma::Subsampled, apple_media: false, rdp_graphics: false, rdp_h264: false });
        assert_eq!(plan.describe(), "video q80 4:2:0");
    }

    /// The floor key is gone: a settle sharpens a quiet desktop at the dial, which
    /// is what the floor was for, and a file that still writes it is refused as
    /// any unknown key is.
    #[test]
    fn a_render_floor_is_no_longer_a_key() {
        let err = parse_target("video_quality = 80\nrender_adaptive_min = 30").unwrap_err();
        assert!(format!("{err:#}").contains("render_adaptive_min"), "{err:#}");
    }

    /// The audio keys resolve the same way the render dial does: defaults
    /// filled, kilobits become bits, and the walk is on unless it was turned
    /// off — a target that names none of them already adapts.
    #[test]
    fn the_audio_plan_resolves_defaults_and_the_walk() {
        let cfg = parse_audio_target("").expect("bare audio");
        assert_eq!(cfg.targets[0].audio_plan(), AudioPlan::default());
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan { bitrate_bps: 96_000, adaptive: true },
            "adaptive by default, from the default ceiling"
        );

        let cfg = parse_audio_target("audio_bitrate = 128").expect("a rate");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan { bitrate_bps: 128_000, adaptive: true },
            "a ceiling alone moves the ceiling and keeps the walk"
        );

        let cfg = parse_audio_target("audio_adaptive = false").expect("fixed");
        assert_eq!(cfg.targets[0].audio_plan(), AudioPlan::fixed(), "turned off, the plan has no walk");
        assert_eq!(cfg.targets[0].audio_plan(), AudioPlan { bitrate_bps: 96_000, adaptive: false });

        let cfg = parse_audio_target("audio_bitrate = 64\naudio_adaptive = true").expect("adaptive at a rate");
        assert_eq!(cfg.targets[0].audio_plan(), AudioPlan { bitrate_bps: 64_000, adaptive: true });
    }

    /// A ceiling under the walk's floor is no contradiction: the plan keeps
    /// the walk, which then has nothing to give up, the way the render dial
    /// does under its encoder's floor.
    #[test]
    fn a_ceiling_below_the_floor_keeps_a_walk_of_nothing() {
        let cfg = parse_audio_target("audio_bitrate = 24").expect("a low ceiling");
        let plan = cfg.targets[0].audio_plan();
        assert_eq!(plan, AudioPlan { bitrate_bps: 24_000, adaptive: true });
        assert!(plan.bitrate_bps < sound_opus::walk::BITRATE_FLOOR as i32);
    }

    /// The floor key is gone: the walk's floor is sound-opus's, fixed where a
    /// lower rate stops being the same sound, and a file that still writes it
    /// is refused as any unknown key is.
    #[test]
    fn an_audio_floor_is_no_longer_a_key() {
        let err = parse_audio_target("audio_adaptive_min = 24").unwrap_err();
        assert!(format!("{err:#}").contains("audio_adaptive_min"), "{err:#}");
    }

    /// Every key that tunes the encoder is refused on a target none of whose
    /// sessions has sound to encode, and so is the adaptive switch in either
    /// position.
    #[test]
    fn the_bitrate_keys_need_a_target_that_carries_sound() {
        let plain = |key: &str| ConfigFile::parse(&vnc_toml(key)).unwrap_err();
        let err = plain("audio_bitrate = 96");
        assert!(format!("{err:#}").contains("is plain vnc and sets audio_bitrate"), "{err:#}");
        for switch in ["true", "false"] {
            let err = plain(&format!("audio_adaptive = {switch}"));
            assert!(format!("{err:#}").contains("sets audio_adaptive"), "{err:#}");
        }
    }

    /// The bitrate has a range: libopus's, in kbit/s.
    #[test]
    fn the_audio_bitrate_is_validated_against_libopus() {
        parse_audio_target("audio_bitrate = 6").expect("the bottom of the range");
        parse_audio_target("audio_bitrate = 510").expect("the top of the range");
        for off in ["audio_bitrate = 5", "audio_bitrate = 999"] {
            let err = parse_audio_target(off).unwrap_err();
            assert!(format!("{err:#}").contains("6–510"), "{err:#}");
        }
    }
}
