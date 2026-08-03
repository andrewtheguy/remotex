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
    /// Apple's native pasteboard is available, while pixels stay raw so this is
    /// the uncompressed alternative to Apple's record-layer subtype.
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
    /// the dynamic-resolution path is the least settled part.
    ///
    /// High Performance Screen Sharing uses a virtual display rather than the
    /// Mac's physical displays. This gateway requests one virtual display at the
    /// target's [`TargetConfig::width`] and [`TargetConfig::height`], adds zlib
    /// pixels, and uses Apple's encrypted record transport.
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
    /// per cell or — under [`MotionSubtype::H264`] — an H.264 stream per coalesced
    /// moving region.
    ///
    /// A cell that stops changing is re-sent once at the base encode, so a paused
    /// screen returns to full quality on its own: the base is the truth, motion is
    /// a temporary discount on what is too busy to notice.
    Motion,
    /// The whole desktop as one H.264 stream, at a fixed quality
    /// ([`TargetConfig::render_quality`]).
    ///
    /// Not a codec on the [`RenderSubtype`] axis, and deliberately not: the other
    /// three are *per-tile* codecs, where every tile is independent, reorderable,
    /// cacheable and droppable once something covers it. An H.264 access unit is none
    /// of those — it is one link in a chain, and losing any link corrupts every frame
    /// after it until the next keyframe. So this axis is where it goes, and it
    /// refuses the subtype axis outright rather than pretending to be a fourth value
    /// on it.
    ///
    /// It follows that this is a different *transport*, not a different compressor:
    /// no tiles, no cell grid, no per-region decisions, one access unit per remote
    /// frame. See [`crate::h264`], and note that only the browser decodes it —
    /// `remotex.app` refuses a video target by name.
    Video,
}

/// What a target's redirected audio is carried as, chosen per target because it
/// is a bandwidth-against-processing trade and only the operator knows which side
/// of it a given link is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioCodec {
    /// Opus at 96 kbps in 20 ms packets ([`crate::opus_stream`]). The default,
    /// and the right answer for any link that leaves the building: it is well
    /// clear of where stereo Opus starts to be audibly lossy, and 96 kbps is a
    /// fifteenth of what the alternative costs.
    #[default]
    Opus,
    /// The remote's own PCM, unencoded and unresampled ([`crate::pcm_stream`]):
    /// 1.41 Mbit/s, no encoder in the gateway and no decoder in the client.
    ///
    /// For a fast local network, where those megabits are free and the thing
    /// worth removing is everything that touches a sample. It is also the only
    /// option that plays without WebCodecs, and therefore the only one that
    /// works over plain `http://` to a host that is not `localhost`.
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
/// question at quality 60 as at 10. And this is the axis `h264` appears on, where
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
    /// An H.264 stream per coalesced moving region, at
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
    H264,
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

/// The whole render dial as an engine sees it, and the one place the two ways this
/// gateway can put a desktop on a wire are told apart.
///
/// An enum rather than a struct with a flag, because the difference is not a setting:
/// [`Self::Tiles`] cuts damage into independent images, and [`Self::Video`] feeds one
/// stateful stream. Nothing sensible is shared between those two paths, and making it
/// an enum is what stops a consumer from quietly handling only the first.
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
    /// An H.264 stream per coalesced moving region, at this 1–100 quality.
    ///
    /// The quality is the dial rather than a quantizer: turning that into one is
    /// [`crate::h264`]'s business, and it is the only module that should know what a
    /// quantizer is.
    Stream { quality: u8 },
}

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
    },
    /// The whole framebuffer as one H.264 stream at a fixed quantizer.
    ///
    /// The quality is the 1–100 dial rather than a quantizer: turning that into one
    /// is [`crate::h264`]'s business, and it is the only module that should know what
    /// a quantizer is.
    Video { quality: u8 },
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
    /// more than one: `subtype = "ard"` on a `vnc` target is Apple Screen
    /// Sharing Standard mode. Unset means the protocol's ordinary form.
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
    /// size this end considers right. A generic or Standard-mode VNC server keeps
    /// its own size at connect and this is only ever consulted for a client that
    /// asks. For `ard-high-performance`, it is the virtual display's requested
    /// mode.
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
    /// desktop size when the user asks for one. Whether the window may *also* drive
    /// it unasked is a second question, and not this one: see
    /// [`Self::auto_resize`].
    ///
    /// On RDP this also turns on density matching, because there a density *is* a
    /// resize: the Display Control channel this negotiates is the only way to tell
    /// a live session to render at 200%, so a Retina client gets twice the pixels
    /// and a UI drawn twice as large. Off, an RDP target ignores the client's
    /// density entirely.
    ///
    /// On `ard-high-performance`, this lets viewport reports replace the virtual
    /// display's one-mode dynamic configuration. The setup descriptor itself
    /// always enables the Mac's dynamic geometry; this flag remains the operator's
    /// permission for clients to change it after connect. Standard `ard` refuses
    /// the option because it exposes physical displays.
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
    /// Which codec [`Self::audio`] encodes with; `None` reads as
    /// [`AudioCodec::Opus`]. `Option` rather than a bare default so that setting
    /// it on a target that never enabled audio is refused at parse time instead
    /// of accepted and left inert.
    #[serde(default)]
    pub audio_codec: Option<AudioCodec>,
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
    /// `h264` is never a default: it is not a cheaper still but a stream per moving
    /// region, so it has to be asked for.
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
}

impl TargetConfig {
    /// Whether a client may let its window drive this target's size *unasked* —
    /// the "auto resize" both clients offer — as opposed to resizing when the user
    /// asks for it, which is [`Self::resize`] and nothing more.
    ///
    /// Plain `vnc` only, and deliberately not a config key: it is a statement about
    /// which engines survive a stream of resizes, which the operator has no way to
    /// know and no way to change. DesktopSize/ExtendedDesktopSize renegotiation is
    /// the one resize path here that costs nothing but a new framebuffer.
    ///
    /// The two that are excluded each have a fault in
    /// [`docs/known-issues.md`](../docs/known-issues.md), and both are reached far
    /// more often by a window that reports continuously than by a person pressing a
    /// button: RDP answers a real size change with a Deactivation-Reactivation
    /// Sequence that sometimes ends the session, and `ard-high-performance`
    /// renegotiates a virtual display that can be left wrong for the rest of the
    /// session. Standard `ard` refuses `resize` outright and so never reaches here
    /// with it set.
    ///
    /// Manual resize stays available on all of them. A fault the user provoked, once,
    /// with a visible cause is a different thing from one a window drag walks into.
    pub fn auto_resize(&self) -> bool {
        self.resize && self.protocol == Protocol::Vnc && self.subtype.is_none()
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
        if let (RenderType::Video, Some(quality)) = (self.render_type, self.render_quality) {
            return RenderPlan::Video { quality };
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
                Some(MotionSubtype::H264) => Some(MotionEncode::Stream { quality: q }),
                None => None,
            },
            _ => None,
        };
        RenderPlan::Tiles { base, motion, debug: self.render_motion_debug }
    }

    /// The motion encode this target asked for, falling back to the base codec when
    /// the key is omitted — which `h264` never is, since a stream is not a cheaper
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
///   secret and the web root decided by the app, so a `[server]` block could only
///   contradict it — and it must come up with **no targets at all**, because that
///   is what a first launch has and the picker's job is to say so.
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
    /// The loopback port `remotex.app`'s own gateway listens on
    /// ([`crate::embedded::DEFAULT_PORT`] when absent). [`Audience::Embedded`] only:
    /// a served gateway spells this `[server].port`, and accepting both spellings
    /// would be two places to write one value.
    ///
    /// Top-level for the reason `branding` is — the embedded config has no
    /// `[server]` block to put it in — and it is here at all because the port is the
    /// one thing about that gateway a user can be forced to change: it is fixed
    /// rather than ephemeral so the page's origin holds still across launches, and
    /// two instances running at once therefore collide on it.
    #[serde(default)]
    pub port: Option<u16>,
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
    /// disk. Defaults to [`default_static_dir`] for a served gateway; an embedded
    /// one is told where its bundle keeps it (`--web-root`).
    ///
    /// Every gateway has one, because every client is the same SPA: the browser
    /// loads it over the network and `remotex.app` loads it from loopback.
    pub static_dir: PathBuf,
    /// Every target profile this process serves; the post-login picker selects
    /// one. Non-empty for [`Audience::Served`]; possibly empty for an embedded
    /// gateway, whose client shows "no targets are configured" instead.
    pub targets: Vec<TargetConfig>,
    /// What gets a request past the door: a login, or the embedded client's token.
    pub auth: GatewayAuth,
    /// Display name for the login screen, interstitials, and browser tab title.
    pub branding: String,
    /// The `<label>.localhost` name this gateway's origin is spelled with. `None`
    /// disables the redirect entirely.
    ///
    /// Two audiences reach it and they want it for the same reason, which is why the
    /// field is not named after either: a served gateway sets it from
    /// `[server].dev_subdomain` so two development gateways on one machine stop
    /// sharing a cookie jar, and an embedded one is *always* given one, derived from
    /// its instance directory, so the origin of the page `remotex.app` shows is a
    /// fact about that instance rather than about which port the kernel happened to
    /// hand out (see [`crate::embedded::Instance::origin_label`]).
    ///
    /// Stored as the whole hostname rather than the label so the one place that
    /// validated it is the only place that builds it — a redirect target
    /// assembled at the point of use is one that can be assembled wrongly.
    pub loopback_hostname: Option<String>,
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
                 the app serves no web UI and authenticates itself. Only branding, \
                 port and [[targets]] belong here"
            );
        } else {
            anyhow::ensure!(
                !config.targets.is_empty(),
                "config has no [[targets]] — at least one target profile is required"
            );
            // The mirror of the refusal above, and refused for the same reason: this
            // key is the embedded gateway's spelling of a value a served one already
            // has under `[server]`. Silently preferring one of the two would make the
            // other read as configuration and behave as decoration.
            anyhow::ensure!(
                config.port.is_none(),
                "top-level `port` belongs to remotex.app's own config; a served \
                 gateway sets [server].port"
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
            // Same rule one step down: a codec for audio that was never turned on
            // is a key that could not do anything, and the likely typo behind it
            // is a forgotten `audio = true` rather than a deliberate choice.
            anyhow::ensure!(
                target.audio_codec.is_none() || target.audio,
                "target {:?} sets audio_codec but not audio, so nothing would encode",
                target.name
            );
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
                    anyhow::ensure!(
                        subtype != Subtype::ArdHighPerformance
                            || (target.width != 0 && target.height != 0),
                        "target {:?} is subtype {name:?} and requests a virtual display at \
                         {}×{}, but width and height must both be greater than zero",
                        target.name,
                        target.width,
                        target.height
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
                         a PNG base must name its own: \"jpeg\", \"webp\", or \"h264\" for a \
                         stream per moving region",
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
                // stateful H.264 stream carrying the whole framebuffer, so there is
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
                     H.264 stream, where every frame depends on the one before it. Drop \
                     render_subtype to keep \"video\", or set render_type = \"fixed-quality\" \
                     to keep the codec",
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
                     encode. Under render_motion_subtype = \"h264\" it is the quality each \
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
        }
        Ok(config)
    }

    /// Resolve the runtime configuration of the gateway inside `remotex.app`:
    /// loopback, an ephemeral port, the SPA out of the app's bundle, and a freshly
    /// minted token.
    ///
    /// Every one of those is an argument here rather than a default that
    /// `[server]` could override, which is what [`Audience::Embedded`] enforces on
    /// the way in. `branding` is the one thing such a config *may* say about the
    /// gateway itself, because it is about the app rather than about the server: it
    /// names a window, not a deployment, and two instances on one Mac are easier to
    /// tell apart if they can be called different things.
    pub fn resolve_embedded(
        self,
        token: EmbeddedToken,
        web_root: PathBuf,
        port: u16,
        origin_label: &str,
    ) -> anyhow::Result<AppConfig> {
        Ok(AppConfig {
            // Not `localhost`: that name resolves to both loopbacks, and binding one
            // address keeps this a single socket. The *name* the client is given
            // resolves to both — `<label>.localhost` — and a client that picks the
            // v6 address falls straight back to this one, which was measured rather
            // than assumed.
            host: "127.0.0.1".to_owned(),
            port,
            static_dir: web_root,
            targets: self.targets,
            auth: GatewayAuth::Token(token),
            branding: Self::resolve_branding(self.branding.as_deref()),
            // Always a name, never `None`, and that is the difference between the two
            // audiences: for a served gateway this is an opt-in development
            // convenience, and here it is what gives the page an origin that survives
            // a relaunch. See the field.
            loopback_hostname: Some(loopback_hostname(origin_label)?),
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
            static_dir: server.static_dir.unwrap_or_else(default_static_dir),
            // Non-empty is guaranteed by `parse`.
            targets: self.targets,
            auth: GatewayAuth::Login(site_passwd),
            branding: Self::resolve_branding(self.branding.as_deref()),
            loopback_hostname: server
                .dev_subdomain
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(loopback_hostname)
                .transpose()
                .context("invalid [server].dev_subdomain")?,
        })
    }
}

/// `<label>.localhost`, refusing anything that is not a single DNS label.
///
/// By RFC 6761 every name under `.localhost` is loopback, and macOS resolves them —
/// to `::1` and `127.0.0.1` both. Two such names are two *origins*, which is the
/// property both callers are after: separate cookie jars and separate
/// `localStorage`, on one machine and one port.
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
pub fn loopback_hostname(label: &str) -> anyhow::Result<String> {
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
            resolved(r#"dev_subdomain = "a""#).loopback_hostname.as_deref(),
            Some("a.localhost")
        );
        // Unset, and whitespace-only, both disable it — as `branding` does.
        assert_eq!(resolved("").loopback_hostname, None);
        assert_eq!(resolved(r#"dev_subdomain = "  ""#).loopback_hostname, None);
        // Trimmed, so a stray space cannot become part of a hostname.
        assert_eq!(
            resolved(r#"dev_subdomain = "  b  ""#).loopback_hostname.as_deref(),
            Some("b.localhost")
        );
        // Digits and inner hyphens are legal in a DNS label.
        assert_eq!(
            resolved(r#"dev_subdomain = "gw-2""#).loopback_hostname.as_deref(),
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
                .loopback_hostname
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
            RenderPlan::Tiles { base: TileCodec::Png, motion: None, debug: false }
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
            RenderPlan::Tiles { base: TileCodec::Jpeg(60), motion: None, debug: false }
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
            RenderPlan::Tiles { base: TileCodec::Webp(50), motion: None, debug: false }
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
        assert_eq!(cfg.targets[0].render_plan(), RenderPlan::Video { quality: 60 });
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
            assert!(msg.contains("H.264"), "the message should say what video is: {msg}");
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
                debug: false
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
                debug: false
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
                debug: false
            }
        );
    }

    /// The flagship pairing for a stream per moving region: a lossless base, so the
    /// text beside a video is exact and never re-encoded, and H.264 carrying only
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
            render_motion_subtype = "h264"
            render_motion_quality = 30
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.targets[0].render_plan(),
            RenderPlan::Tiles {
                base: TileCodec::Png,
                motion: Some(MotionEncode::Stream { quality: 30 }),
                debug: false
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
            render_motion_subtype = "h264"
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
            RenderPlan::Tiles { base: TileCodec::Webp(60), motion: None, debug: false }
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
        assert_eq!((target.width, target.height), (1600, 1000));
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

    /// `resize` is permission to resize when asked; letting the window drive it is
    /// a second permission the gateway decides, and only plain `vnc` has it. Each
    /// engine that is refused it is named here, so removing one from the rule has
    /// to be a deliberate edit to this list.
    #[test]
    fn only_plain_vnc_may_be_resized_by_the_window() {
        let plain = &ConfigFile::parse(&vnc_toml("resize = true")).unwrap().targets[0];
        assert!(plain.resize && plain.auto_resize());

        // High Performance may be resized when asked and never by the window: a
        // viewport report replaces the virtual display's mode, and doing that on
        // every drag is how the desktop is left wrong (docs/known-issues.md).
        let hp = &ConfigFile::parse(&vnc_toml(
            "subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"\n\
             width = 1600\nheight = 1000\nresize = true",
        ))
        .unwrap()
        .targets[0];
        assert!(hp.resize && !hp.auto_resize());

        // RDP the same, for the reactivation a real size change costs.
        let rdp = &ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "pc"
            protocol = "rdp"
            host = "192.0.2.10"
            resize = true
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .targets[0];
        assert!(rdp.resize && !rdp.auto_resize());

        // And neither permission without the operator's, which is what keeps this
        // from becoming "plain vnc always follows the window".
        let off = &ConfigFile::parse(&vnc_toml("")).unwrap().targets[0];
        assert!(!off.resize && !off.auto_resize());
    }

    #[test]
    fn the_high_performance_virtual_display_requires_nonzero_dimensions() {
        for dimensions in ["width = 0\nheight = 1000", "width = 1600\nheight = 0"] {
            let err = ConfigFile::parse(&vnc_toml(&format!(
                "subtype = \"ard-high-performance\"\nusername = \"andrew\"\npassword = \"h\"\n{dimensions}"
            )))
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("width and height must both be greater than zero"),
                "{err:#}"
            );
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

    /// An audio target that says nothing about the codec gets Opus, and one that
    /// asks for passthrough gets it. The default is the load-bearing half: it is
    /// what keeps every existing config encoding exactly what it encoded before.
    #[test]
    fn the_audio_codec_defaults_to_opus_and_can_be_asked_for() {
        let config = ConfigFile::parse(&format!(
            r#"
            [server]
            {}

            [[targets]]
            name = "quiet"
            protocol = "rdp"
            host = "10.0.0.5"
            audio = true

            [[targets]]
            name = "on-the-lan"
            protocol = "rdp"
            host = "10.0.0.6"
            audio = true
            audio_codec = "pcm"
            "#,
            site_passwd_line()
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(config.targets[0].audio_codec, None);
        assert_eq!(
            config.targets[0].audio_codec.unwrap_or_default(),
            AudioCodec::Opus,
            "an unset codec reads as Opus"
        );
        assert_eq!(config.targets[1].audio_codec, Some(AudioCodec::Pcm));
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
}
