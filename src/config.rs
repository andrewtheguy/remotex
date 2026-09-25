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
/// business; both current subtypes are `vnc`'s, and both describe the same Mac.
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
    /// Apple's native pasteboard is available, and the rectangles are zlib.
    Ard,
    /// The same Mac in High Performance Screen Sharing, as Apple's viewer has it:
    /// the same wire as [`Subtype::Ard`] on a virtual display, with the picture as
    /// HEVC and the sound as AAC-ELD over the media
    /// stream Screen Sharing negotiates on the RFB connection and sends over UDP
    /// with SRTP ([`crate::vnc_apple_media`]). Zlib rectangles carry the picture
    /// only until the stream does and across display changes; a stream that fails
    /// ends the session, as it ends Apple's viewer's.
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
    /// pinned [`TargetConfig::width`] and [`TargetConfig::height`] when both are
    /// set, or at the connecting client's screen resolution otherwise. Apple's
    /// native pasteboard payloads are carried inside the encrypted record
    /// transport when `clipboard` is enabled. With `resize`, viewport reports
    /// replace the virtual display's one advertised mode and the Mac answers with
    /// its new layout.
    ///
    /// The picture and the sound go together — the Mac refuses one without the
    /// other, and mutes its own output while the sound leg runs — so the target
    /// always carries sound and takes no `audio` key. Only a
    /// build with the `apple-hp-media` feature has the two decoders; any other
    /// refuses the subtype.
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

    /// Whether the picture and sound come over the media stream.
    pub fn media_stream(self) -> bool {
        match self {
            Subtype::Ard => false,
            Subtype::ArdHighPerformance => true,
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

/// What a target's redirected audio is carried as, chosen per target because it
/// is a bandwidth-against-processing trade and only the operator knows which side
/// of it a given link is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioCodec {
    /// Opus in 20 ms packets ([`crate::opus_stream`]), variable-rate, holding
    /// the average at [`TargetConfig::audio_bitrate`] (default 96 kbit/s) or
    /// whatever the adaptive walk has moved it to. The default codec, and the
    /// right answer for any link that leaves the building: the default rate is
    /// well clear of where stereo Opus starts to be audibly lossy, and a
    /// fifteenth of what the alternative costs.
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
    /// The Opus target bitrate — the average the encoder holds to, and the
    /// ceiling of the walk when the plan is adaptive. Carried but unread for
    /// [`AudioCodec::Pcm`], whose whole point is that no encoder exists to give
    /// it to.
    pub bitrate_bps: i32,
    /// `Some(floor)` exactly when the bitrate should track the audio socket's
    /// backpressure, walking between the floor and [`Self::bitrate_bps`] — and
    /// silence should be shed while the link is behind. See
    /// [`TargetConfig::audio_adaptive`].
    pub adaptive_floor_bps: Option<i32>,
}

impl AudioPlan {
    /// `codec` at the default rate with no walk — what `audio_adaptive = false`
    /// resolves to, and the plan a codec with no encoder always gets.
    pub fn fixed(codec: AudioCodec) -> Self {
        Self { codec, adaptive_floor_bps: None, ..Self::default() }
    }
}

impl Default for AudioPlan {
    /// What an unset dial means: Opus at the default rate, walking down to the
    /// default floor when the link is behind. The fallback [`crate::session`]
    /// uses when no target is selected, where there is no config to read.
    fn default() -> Self {
        Self {
            codec: AudioCodec::Opus,
            bitrate_bps: DEFAULT_AUDIO_BITRATE_KBPS as i32 * 1000,
            adaptive_floor_bps: Some(DEFAULT_AUDIO_ADAPTIVE_MIN_KBPS as i32 * 1000),
        }
    }
}

/// The render dial as an engine sees it: the whole framebuffer as one VP9 stream,
/// resolved from a target's stream keys by [`TargetConfig::render_plan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderPlan {
    /// The 1–100 dial the stream holds on a link that can carry it, rather than a
    /// quantizer: turning that into one is [`crate::vp9`]'s business, and it is the
    /// only module that should know what a quantizer is.
    pub quality: u8,
    /// The floor of the adaptive quality walk, when
    /// [`TargetConfig::render_adaptive`] asked for one. `None` keeps the congestion
    /// walk's historical shape: pressure-only, floored at 1.
    pub adaptive: Option<u8>,
    /// [`TargetConfig::render_chroma`], resolved.
    pub chroma: Chroma,
}

impl RenderPlan {
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
        // Always named, because with `auto` the default there is no chroma a card
        // may leave unsaid: an unnamed one would read as 4:2:0 selected on a
        // session that is 4:2:0 only because this browser declined profile 1. What
        // the slot says is the profile on the wire — or, for a config card,
        // whatever `chroma_slot` puts there instead.
        let chroma = match chroma_slot {
            Some(slot) => slot.to_owned(),
            None => self.chroma.card_name().to_owned(),
        };
        // The floor as a suffix: the quality named before it is a ceiling the link
        // may fall below, and this is how far.
        let floor = self.adaptive.map_or_else(String::new, |floor| format!(" · adaptive ≥{floor}"));
        format!("video q{} {chroma}{floor}", self.quality)
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
    /// On `ard-high-performance` the setup descriptor
    /// always enables the Mac's dynamic geometry; this flag decides only whether the window keeps
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
    /// [`Self::audio`]. Refused on every Apple subtype: `ard` carries no sound,
    /// and `ard-high-performance` always carries its media stream's.
    #[serde(default, rename = "audio")]
    pub audio_key: Option<bool>,
    /// Carry the remote's sound. Packets are sent only while the attached client
    /// subscribes. RDP negotiates it at connect (MS-RDPEA); a plain `vnc` target
    /// asks a generic server for wlshare's audio extension, FLAC on the RFB
    /// connection, and is answered by wlshare — see [`crate::vnc_audio`]. Both
    /// opt in with `audio = true`. An `ard` target never carries sound: Standard
    /// mode never touches the Mac's sound output. An `ard-high-performance` target
    /// always does, its media stream's ([`crate::vnc_apple_media`]).
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
    /// The Opus bitrate this target's sound holds on a link that can carry it,
    /// in kbit/s (6–510); `None` reads as [`DEFAULT_AUDIO_BITRATE_KBPS`].
    ///
    /// The audio dial's `video_quality`: a *ceiling* rather than a promise. It
    /// is the average the encoder holds to — Opus is variable-rate, so a packet
    /// of silence costs a few bytes and a packet of music costs about this —
    /// and, with [`Self::audio_adaptive`] on, the rate a link that keeps up gets
    /// and the one the walk climbs back to. Opus only: passthrough PCM has no
    /// encoder to give a rate to, so the key is refused beside
    /// `audio_codec = "pcm"`.
    #[serde(default)]
    pub audio_bitrate: Option<u32>,
    /// Let [`Self::audio_bitrate`] track the audio socket's own backpressure —
    /// on unless the operator turned it off, like [`Self::render_adaptive`].
    ///
    /// A send that blocks means the previous packets are still unwritten, and
    /// sustained blocking walks the bitrate down toward
    /// [`Self::audio_adaptive_min`]; a clear stretch walks it back up to the
    /// ceiling. While behind, wave buffers that are pure silence are shed instead
    /// of queued — silence is the one content whose loss is free, and dropping it
    /// is how the client catches up without a trimmed or resampled note anywhere
    /// (see [`crate::audio`]). Opus only, for the same reason as
    /// [`Self::audio_bitrate`]: writing it either way beside `pcm` is refused.
    ///
    /// Resolved by the accessor of the same name.
    #[serde(default)]
    pub audio_adaptive: Option<bool>,
    /// Floor in kbit/s for [`Self::audio_adaptive`] (6–510, below the
    /// bitrate ceiling); `None` reads as [`DEFAULT_AUDIO_ADAPTIVE_MIN_KBPS`],
    /// or as [`Self::audio_bitrate`] where the ceiling sits below it — a default
    /// floor never narrows a walk to nothing. Refused beside
    /// `audio_adaptive = false`: a floor for a walk that never moves is a key
    /// that could not do anything.
    #[serde(default)]
    pub audio_adaptive_min: Option<u32>,
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
    /// is [`Self::render_adaptive_min`]. The stream already gives quality up when
    /// queueing a frame blocks; this adds the client's own lag — how long the
    /// oldest unacknowledged paint batch has been owed, beyond the link's measured
    /// floor — as a second reason to, and moves the walk's floor up from 1.
    ///
    /// Resolved by the accessor of the same name.
    #[serde(default)]
    pub render_adaptive: Option<bool>,
    /// Floor (1–100) for [`Self::render_adaptive`]; `None` reads as
    /// [`DEFAULT_RENDER_ADAPTIVE_MIN`], or as [`Self::video_quality`] where the dial
    /// sits below it — a default floor never narrows a stream's walk to nothing.
    /// Must not exceed [`Self::video_quality`] when written —
    /// a floor above the ceiling is a contradiction better refused than resolved.
    /// Refused beside `render_adaptive = false`.
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

/// The Opus bitrate (kbit/s) when [`TargetConfig::audio_bitrate`] is unset — the
/// ceiling of the adaptive walk, not a promise. Well clear of where stereo Opus
/// starts to be audibly lossy, and the walk is what takes it down on a link that
/// cannot carry it.
pub const DEFAULT_AUDIO_BITRATE_KBPS: u32 = 96;

/// The adaptive floor (kbit/s) when [`TargetConfig::audio_adaptive_min`] is
/// unset. 32 kbit/s stereo Opus is degraded but continuous — and continuity is
/// the whole point of giving bitrate up.
pub const DEFAULT_AUDIO_ADAPTIVE_MIN_KBPS: u32 = 32;

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
    /// `decoder` is the most colour the attached browser said its `VideoDecoder`
    /// takes, carried on the session socket and held with its attachment
    /// ([`crate::session::SessionManager::attach`]). It is read by
    /// [`ChromaChoice::Auto`] and by nothing else: a target that names a profile
    /// gets that profile whatever this says, which is what keeps the explicit key a
    /// decision no browser can overrule.
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
        RenderPlan { quality, adaptive, chroma }
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

    /// Whether the Opus bitrate walks with the link — on unless the operator
    /// wrote `audio_adaptive = false`. Answers for the key alone: a passthrough
    /// target has no rate to walk, and [`Self::audio_plan`] is what says so.
    pub fn audio_adaptive(&self) -> bool {
        self.audio_adaptive.unwrap_or(true)
    }

    /// The audio keys collapsed to what the encoder is built from, the same way
    /// [`Self::render_plan`] collapses the render dial: defaults resolved,
    /// kilobits turned into the bits libopus speaks, and the adaptive floor
    /// present exactly when there is a walk — Opus, and the operator did not
    /// turn it off. Callers gate on [`Self::audio`] — a target without audio has
    /// no plan to resolve.
    pub fn audio_plan(&self) -> AudioPlan {
        let codec = self.audio_codec.unwrap_or_default();
        let bitrate_kbps = self.audio_bitrate.unwrap_or(DEFAULT_AUDIO_BITRATE_KBPS);
        // The floor the walk will hold to, never above the ceiling it walks under.
        // Only the *default* floor can sit there — an explicit `audio_adaptive_min`
        // over the ceiling is refused at parse — and a low ceiling then gets a walk
        // of nothing rather than a refused config, as `video_quality` does. Held
        // here so a card cannot state a floor the stream never walks down to.
        let adaptive_floor_bps = (codec == AudioCodec::Opus && self.audio_adaptive()).then(|| {
            self.audio_adaptive_min
                .unwrap_or(DEFAULT_AUDIO_ADAPTIVE_MIN_KBPS)
                .min(bitrate_kbps) as i32
                * 1000
        });
        AudioPlan { codec, bitrate_bps: bitrate_kbps as i32 * 1000, adaptive_floor_bps }
    }

    /// The one PCM format this target's wave buffers can be in, known before the
    /// remote has said anything: what the RDP engine asks a server to redirect
    /// ([`crate::audio::PCM_CD_QUALITY`]), what a Mac's media stream decodes to
    /// ([`crate::vnc_apple_media::AUDIO_FORMAT`]), or what a generic VNC server
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
            Protocol::Vnc if self.media_stream() => crate::vnc_apple_media::AUDIO_FORMAT,
            Protocol::Vnc => crate::vnc_audio::SOURCE_FORMAT,
        }
    }

    /// Whether this target's picture and sound come over the Mac's media stream:
    /// `ard-high-performance` ([`crate::vnc_apple_media`]).
    pub fn media_stream(&self) -> bool {
        self.protocol == Protocol::Vnc && self.subtype.is_some_and(Subtype::media_stream)
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
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
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
            // The media stream's decoders are the `apple-hp-media` feature's.
            anyhow::ensure!(
                !target.media_stream() || cfg!(feature = "apple-hp-media"),
                "target {:?} is subtype \"ard-high-performance\", and this remotex was built \
                 without the apple-hp-media feature, which has its decoders. Build with \
                 `--features apple-hp-media`, or use subtype \"ard\".",
                target.name
            );
            // A Mac's sound is not the target's to turn on or off: Standard mode
            // never touches it, and High Performance's media stream always carries
            // its own.
            let apple = target.protocol == Protocol::Vnc && target.subtype.is_some();
            anyhow::ensure!(
                !(apple && target.audio_key.is_some()),
                "target {:?} sets audio on an {} target, whose sound is not the target's to \
                 switch: ard carries no sound and leaves the Mac's output alone (it can \
                 play to an AirPlay receiver outside remotex), and ard-high-performance \
                 always carries the Mac's sound itself. Remove the key.",
                target.name,
                target.subtype.map_or("apple", Subtype::name)
            );
            target.audio = target.media_stream() || (!apple && target.audio_key.unwrap_or(false));
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
            // A pinned size is asked for as pixels at 1x, so the one oversize the
            // video stream refuses that check-config *can* see is a pin already
            // past the picture ceiling: at runtime the engines hold a screen under
            // it, but holding a pin would open at a size the operator did not
            // choose. (A pin under the ceiling at 1x may still land over it on a
            // 2x screen; that one is held, like a screen.)
            anyhow::ensure!(
                target.pinned_size().is_none_or(|(w, h)| {
                    crate::video::within_ceiling((u32::from(w), u32::from(h)))
                }),
                "target {:?} pins a {:?}×{:?} size, but the video stream encodes at most a \
                 long side of {} and a short side of {} — pin a smaller size, or leave the \
                 pin out",
                target.name,
                target.width,
                target.height,
                crate::video::MAX_LONG_SIDE,
                crate::video::MAX_SHORT_SIDE
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
            // extension on a generic VNC target ([`crate::vnc_audio`]), and High
            // Performance's media stream on `ard-high-performance`
            // ([`crate::vnc_apple_media`]). `ard` carries none. The Apple
            // subtypes are checked above.
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
            // Either way: `false` beside pcm is as unreadable as `true`, and a key
            // nothing reads is a mistake to report, not a preference to keep.
            anyhow::ensure!(
                target.audio_adaptive.is_none() || opus,
                "target {:?} sets audio_adaptive, which only an opus audio target uses — \
                 adapting means moving the encoder's bitrate, and this target has no opus \
                 encoder",
                target.name
            );
            anyhow::ensure!(
                target.audio_adaptive_min.is_none() || (opus && target.audio_adaptive()),
                "target {:?} sets audio_adaptive_min beside audio_adaptive = false or no \
                 opus encoder — the floor belongs to the adaptive walk, and without the \
                 walk nothing would read it",
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
            if let Some(kbps) = target.audio_adaptive_min {
                anyhow::ensure!(
                    (6..=510).contains(&kbps),
                    "target {:?} sets audio_adaptive_min = {kbps}, which is out of range — \
                     it is in kbit/s and must be 6–510",
                    target.name
                );
                anyhow::ensure!(
                    kbps < bitrate,
                    "target {:?} sets audio_adaptive_min = {kbps} at or above the bitrate \
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

    /// A target with no stream keys streams the whole desktop at the default dial,
    /// with the adaptive walk on and the browser choosing the chroma.
    #[test]
    fn a_bare_target_streams_at_the_defaults() {
        let cfg = parse_target("").expect("a bare target");
        for decoder in [Chroma::Subsampled, Chroma::Full] {
            assert_eq!(
                cfg.targets[0].render_plan(decoder),
                RenderPlan {
                    quality: DEFAULT_VIDEO_QUALITY,
                    adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                    chroma: decoder,
                }
            );
        }
    }

    #[test]
    fn a_video_quality_is_the_streams_dial() {
        let cfg = parse_target("video_quality = 60").expect("a quality");
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan {
                quality: 60,
                adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
                chroma: Chroma::Subsampled
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
        let video = |extra: &str, decoder| {
            parse_target(&format!("video_quality = 100\n{extra}")).unwrap().targets[0]
                .render_plan(decoder)
        };
        let stream = |chroma| RenderPlan {
            quality: 100,
            adaptive: Some(DEFAULT_RENDER_ADAPTIVE_MIN),
            chroma,
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
        assert_eq!(summary(""), "video q60 chroma auto · adaptive ≥20");
        assert_eq!(summary("render_chroma = \"auto\""), summary(""));
        assert_eq!(summary("render_chroma = \"420\""), "video q60 4:2:0 · adaptive ≥20");
        assert_eq!(summary("render_chroma = \"444\""), "video q60 4:4:4 · adaptive ≥20");
        assert_eq!(summary("render_adaptive = false"), "video q60 chroma auto");
    }

    /// The session card names the resolved plan: the dial, the chroma on the wire,
    /// and the floor where the walk runs.
    #[test]
    fn a_session_card_describes_the_resolved_stream() {
        let describe = |keys: &str, decoder| {
            parse_target(keys).unwrap().targets[0].render_plan(decoder).describe()
        };
        assert_eq!(describe("video_quality = 60", Chroma::Subsampled), "video q60 4:2:0 · adaptive ≥20");
        assert_eq!(describe("video_quality = 60", Chroma::Full), "video q60 4:4:4 · adaptive ≥20");
        assert_eq!(
            describe("video_quality = 60\nrender_chroma = \"444\"", Chroma::Subsampled),
            "video q60 4:4:4 · adaptive ≥20"
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
    /// The Apple subtypes this build accepts: High Performance only where the
    /// `apple-hp-media` feature gives it its decoders.
    const APPLE_SUBTYPES: &[&str] = if cfg!(feature = "apple-hp-media") {
        &["ard", "ard-high-performance"]
    } else {
        &["ard"]
    };

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
    #[cfg(feature = "apple-hp-media")]
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

    /// The one oversize check-config can see: a pin the video stream would refuse
    /// at 1x.
    #[test]
    fn a_pinned_size_over_the_video_ceiling_is_refused() {
        let err = ConfigFile::parse(&rdp_toml("width = 5120\nheight = 2880"))
            .expect_err("a 5K pin parsed");
        assert!(format!("{err:#}").contains("3840"), "{err:#}");
        for pin in ["width = 3840\nheight = 2400", "width = 2400\nheight = 3840"] {
            ConfigFile::parse(&rdp_toml(pin))
                .expect("a 4K pin, either way up, is a picture the stream takes");
        }
    }

    /// A zero axis is refused on every target alike — a High Performance
    /// virtual display was merely the first place it was caught misbehaving.
    #[test]
    fn a_pinned_size_requires_nonzero_dimensions() {
        for dimensions in ["width = 0\nheight = 1000", "width = 1600\nheight = 0"] {
            let apple = APPLE_SUBTYPES.iter().map(|subtype| {
                format!("subtype = \"{subtype}\"\nusername = \"andrew\"\npassword = \"h\"\n")
            });
            for subtype in std::iter::once(String::new()).chain(apple) {
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

    /// RDP and generic VNC both take a per-target audio key; `ard` carries no
    /// sound and `ard-high-performance` always carries its own, so both Apple
    /// subtypes refuse it.
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
        for subtype in APPLE_SUBTYPES {
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

    /// Standard mode never touches the Mac's sound, so an `ard` target has none
    /// and refuses the key either way.
    #[test]
    fn a_standard_mac_carries_no_sound() {
        let target = "[[targets]]\nname = \"mac\"\nprotocol = \"vnc\"\nsubtype = \"ard\"\n\
                      host = \"10.0.0.5\"\nusername = \"andrew\"\npassword = \"h\"\n";
        let config = ConfigFile::parse(&format!("[server]\n{}\n{target}", site_passwd_line()))
            .unwrap()
            .resolve()
            .unwrap();
        assert!(!config.targets[0].audio);

        for key in ["audio = true", "audio = false"] {
            let err = ConfigFile::parse(&format!("[server]\n{}\n{target}{key}\n", site_passwd_line()))
                .unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains("sets audio on an ard target"), "{rendered}");
            assert!(rendered.contains("Remove the key"), "{rendered}");
        }
    }

    /// High Performance brings the Mac's sound on its own media stream, beside the
    /// picture: always on, and not the target's to switch.
    #[cfg(feature = "apple-hp-media")]
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
        assert!(mac.audio, "the sound leg comes with the picture");
        assert_eq!(mac.audio_source_format(), crate::vnc_apple_media::AUDIO_FORMAT);

        for key in ["audio = true", "audio = false"] {
            let err = ConfigFile::parse(&format!("[server]\n{}\n{target}{key}\n", site_passwd_line()))
                .unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains("sets audio on an ard-high-performance target"), "{rendered}");
        }

        // Its codec keys are the ones any target with sound takes.
        let pcm = ConfigFile::parse(&format!(
            "[server]\n{}\n{target}audio_codec = \"pcm\"\n",
            site_passwd_line()
        ))
        .unwrap();
        assert_eq!(pcm.targets[0].audio_plan().codec, AudioCodec::Pcm);
    }

    /// A build without the decoders refuses High Performance by name, and says
    /// what to build or use instead.
    #[cfg(not(feature = "apple-hp-media"))]
    #[test]
    fn a_build_without_the_decoders_refuses_high_performance() {
        let err = ConfigFile::parse(&vnc_toml(
            "subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"",
        ))
        .unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("without the apple-hp-media feature"), "{rendered}");
        assert!(rendered.contains("subtype \"ard\""), "{rendered}");
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

    /// The switch resolves into the plan with its floor, and the plan says so.
    #[test]
    fn render_adaptive_resolves_a_floor_into_the_plan() {
        let cfg = parse_target("video_quality = 80\nrender_adaptive = true\nrender_adaptive_min = 35")
            .expect("adaptive video");
        let plan = cfg.targets[0].render_plan(Chroma::Subsampled);
        assert_eq!(plan, RenderPlan { quality: 80, adaptive: Some(35), chroma: Chroma::Subsampled });
        assert_eq!(plan.describe(), "video q80 4:2:0 · adaptive ≥35");
    }

    /// A dial below the default floor takes the floor down with it. The walk is the
    /// operator's, and the widest one a dial of 10 admits runs from 10 to 10 — not
    /// from 20, which is a quality that stream never sends. The card has to say the
    /// same, or it promises a floor nothing walks down to.
    ///
    /// Only the default reaches here: a written `render_adaptive_min` above the dial
    /// is refused at parse ([`a_floor_above_a_ceiling_is_refused`]).
    #[test]
    fn a_dial_below_the_default_floor_is_the_floor() {
        let cfg = parse_target("video_quality = 10").expect("a low dial");
        let plan = cfg.targets[0].render_plan(Chroma::Subsampled);
        assert_eq!(plan, RenderPlan { quality: 10, adaptive: Some(10), chroma: Chroma::Subsampled });
        assert_eq!(plan.describe(), "video q10 4:2:0 · adaptive ≥10");
    }

    /// A target that turned the walk off stays exactly on its dial: no floor in the
    /// plan, and the pressure-only walk the stream had before the key existed.
    #[test]
    fn render_adaptive_false_leaves_the_plan_without_a_floor() {
        let cfg = parse_target("video_quality = 80\nrender_adaptive = false")
            .expect("video with the walk off");
        assert_eq!(
            cfg.targets[0].render_plan(Chroma::Subsampled),
            RenderPlan { quality: 80, adaptive: None, chroma: Chroma::Subsampled }
        );
    }

    /// The floor belongs to the walk; beside a walk that was turned off nothing
    /// reads it.
    #[test]
    fn render_adaptive_min_beside_a_walk_turned_off_is_refused() {
        let err = parse_target("video_quality = 80\nrender_adaptive = false\nrender_adaptive_min = 30")
            .unwrap_err();
        assert!(format!("{err:#}").contains("render_adaptive_min"));
    }

    /// A floor above the stream's quality leaves the walk nowhere to go.
    #[test]
    fn a_floor_above_a_ceiling_is_refused() {
        let err = parse_target("video_quality = 50\nrender_adaptive = true\nrender_adaptive_min = 60")
            .unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to go"));

        // The *default* floor over a low dial is no contradiction — the operator
        // never wrote it. It parses, and [`TargetConfig::render_plan`] resolves the
        // floor to the dial instead ([`a_dial_below_the_default_floor_is_the_floor`]).
        parse_target("video_quality = 10\nrender_adaptive = true")
            .expect("a default floor clamps instead of refusing");
    }

    /// The audio keys resolve the same way the render dial does: defaults
    /// filled, kilobits become bits, and the walk is on unless it was turned
    /// off — a bare `audio = true` already adapts.
    #[test]
    fn the_audio_plan_resolves_defaults_and_the_adaptive_floor() {
        let cfg = parse_audio_target("audio = true").expect("bare audio");
        assert_eq!(cfg.targets[0].audio_plan(), AudioPlan::default());
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan {
                codec: AudioCodec::Opus,
                bitrate_bps: 96_000,
                adaptive_floor_bps: Some(32_000)
            },
            "adaptive by default, between the default ceiling and floor"
        );

        let cfg = parse_audio_target("audio = true\naudio_bitrate = 128").expect("a rate");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan {
                codec: AudioCodec::Opus,
                bitrate_bps: 128_000,
                adaptive_floor_bps: Some(32_000)
            },
            "a ceiling alone moves the ceiling and keeps the walk"
        );

        let cfg = parse_audio_target("audio = true\naudio_adaptive = false").expect("fixed");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan::fixed(AudioCodec::Opus),
            "turned off, the plan has no floor"
        );
        assert_eq!(cfg.targets[0].audio_plan().adaptive_floor_bps, None);

        let cfg = parse_audio_target(
            "audio = true\naudio_bitrate = 64\naudio_adaptive = true\naudio_adaptive_min = 24",
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

        // Passthrough never walks, whatever the default says.
        let cfg = parse_audio_target("audio = true\naudio_codec = \"pcm\"").expect("pcm");
        assert_eq!(cfg.targets[0].audio_plan(), AudioPlan::fixed(AudioCodec::Pcm));
    }

    /// A ceiling under the default floor is no contradiction — the operator never
    /// wrote the floor — so the plan holds the floor to the ceiling instead of
    /// refusing, the way the render dial does.
    #[test]
    fn a_ceiling_below_the_default_floor_is_the_floor() {
        let cfg = parse_audio_target("audio = true\naudio_bitrate = 24").expect("a low ceiling");
        assert_eq!(
            cfg.targets[0].audio_plan(),
            AudioPlan {
                codec: AudioCodec::Opus,
                bitrate_bps: 24_000,
                adaptive_floor_bps: Some(24_000)
            }
        );
    }

    /// Passthrough has no encoder: every key that tunes one is refused beside it,
    /// and so is the adaptive switch in either position.
    #[test]
    fn the_bitrate_keys_are_opus_only() {
        let err = parse_audio_target(
            "audio = true\naudio_codec = \"pcm\"\naudio_bitrate = 96",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("audio_bitrate"));

        for switch in ["true", "false"] {
            let err = parse_audio_target(&format!(
                "audio = true\naudio_codec = \"pcm\"\naudio_adaptive = {switch}"
            ))
            .unwrap_err();
            assert!(format!("{err:#}").contains("audio_adaptive"));
        }

        let err = parse_audio_target(
            "audio = true\naudio_codec = \"pcm\"\naudio_adaptive_min = 24",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("audio_adaptive_min"));

        // And without audio at all, same rule one step up.
        let err = parse_audio_target("audio_bitrate = 96").unwrap_err();
        assert!(format!("{err:#}").contains("audio_bitrate"));
        let err = parse_audio_target("audio_adaptive = false").unwrap_err();
        assert!(format!("{err:#}").contains("audio_adaptive"));
    }

    /// The floor needs the walk, has a range, and must sit under the ceiling.
    #[test]
    fn the_audio_floor_is_validated_against_the_walk_and_the_ceiling() {
        // The walk is on by default, so a bare floor is fine …
        parse_audio_target("audio = true\naudio_adaptive_min = 24").expect("a floor for the default walk");
        // … and refused only beside a walk turned off.
        let err = parse_audio_target("audio = true\naudio_adaptive = false\naudio_adaptive_min = 24")
            .unwrap_err();
        assert!(format!("{err:#}").contains("audio_adaptive_min"));

        let err = parse_audio_target("audio = true\naudio_adaptive_min = 4").unwrap_err();
        assert!(format!("{err:#}").contains("6–510"));

        let err = parse_audio_target("audio = true\naudio_bitrate = 48\naudio_adaptive_min = 48")
            .unwrap_err();
        assert!(format!("{err:#}").contains("nowhere to go"));

        // The *default* floor above a low ceiling is no contradiction — the
        // operator never wrote it. It parses, and the plan clamps it to the
        // ceiling instead ([`a_ceiling_below_the_default_floor_is_the_floor`]).
        parse_audio_target("audio = true\naudio_bitrate = 8")
            .expect("a default floor clamps instead of refusing");

        let err = parse_audio_target("audio = true\naudio_bitrate = 999").unwrap_err();
        assert!(format!("{err:#}").contains("6–510"));
    }
}
