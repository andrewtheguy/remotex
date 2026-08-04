//! VP9 encoding: one stream over a rectangle of a [`crate::video::Mirror`].
//!
//! The default codec, and the reason there is a choice at all. H.264 is the one a browser is not
//! guaranteed to have — a Chromium built without proprietary codecs refuses it, and so does a
//! Firefox on a system with no system decoder — and VP9 is BSD-licensed with a patent grant,
//! which is exactly the property that gets it into every browser build. Which of the two a
//! session uses is [`crate::config::TargetConfig::video_codec`]'s; nothing here knows it was
//! chosen.
//!
//! What the two modules share — the mirror, the coded rectangle, the RGB→I420 conversion, the
//! 1–100 dial — is [`crate::video`]'s. This module is only libvpx.
//!
//! Two things about libvpx's shape are worth knowing before reading:
//!
//! - **It returns error codes rather than asserting**, unlike openh264. So this module needs no
//!   containment argument of the kind [`crate::h264`] carries: every call goes through [`vpx!`],
//!   which turns a bad code into an `anyhow::Error` carrying libvpx's own explanation, and a
//!   failure ends one session instead of the process.
//! - **Its C API is entirely `unsafe` and largely out-parameters.** The invariants are stated at
//!   each call. Two are easy to get wrong and neither announces itself: the image built by
//!   `vpx_img_wrap` borrows the caller's planes and must never reach `vpx_img_free`, and
//!   `vpx_codec_control_` is variadic, so passing the wrong argument type for a control id
//!   compiles and corrupts the stack.

use std::os::raw::{c_int, c_ulong};

use vpx_sys as vpx;

use crate::tiles::Rect;
use crate::video::{
    AccessUnit, I420, Mark, Mirror, QUALITY_MAX, QUALITY_MIN, coded_rect, outline, threads_for,
};

/// Turn a libvpx return code into an `anyhow::Error` naming the call and libvpx's own explanation.
///
/// A macro rather than a function so the call site reads as the C does, and so `what` is a literal
/// at every use. libvpx's `vpx_codec_err_to_string` takes only the code, which is what makes this
/// usable for the calls that have no context yet. Declared before its uses because that is how
/// `macro_rules!` scoping works, and spelled `vpx_sys::` inside rather than through the alias so
/// the expansion does not depend on what the calling scope imported.
macro_rules! vpx {
    ($call:expr, $what:expr) => {{
        let err = $call;
        if err == vpx_sys::vpx_codec_err_t_VPX_CODEC_OK {
            Ok(())
        } else {
            Err(anyhow::anyhow!("vp9 {}: {} ({err})", $what, crate::vp9::detail(err)))
        }
    }};
}

/// libvpx's own words for an error code.
///
/// A safe function rather than an `unsafe` block inside [`vpx!`], because every use of that macro
/// is already inside one and a nested block is a warning that teaches nothing.
fn detail(err: vpx::vpx_codec_err_t) -> String {
    // SAFETY: `vpx_codec_err_to_string` takes a code rather than a context — so any code is
    // valid — and returns a pointer to a static string constant compiled into the archive.
    let detail = unsafe { std::ffi::CStr::from_ptr(vpx::vpx_codec_err_to_string(err)) };
    detail.to_string_lossy().into_owned()
}

/// The quantizer the dial spans. VP9's range is 0–63, coarsest last — the opposite way round
/// from H.264's, which is one reason the congestion loop speaks the dial and not a quantizer.
///
/// The fine end is 8 rather than 0 because VP9 below about 8 is visually lossless on screen
/// content and costs several times the bytes to be so: a dial that mapped past it would have a
/// top third where turning the knob bought nothing but bandwidth. Settled by measurement — see
/// `video::measure_the_encoders`.
const Q_FINEST: u32 = 8;
/// See [`Q_FINEST`]. 63 is VP9's coarsest.
const Q_COARSEST: u32 = 63;

/// libvpx's encoder speed, 0–9, higher being faster and worse.
///
/// 7 is inside the 5–8 band libvpx's own live-encoding guidance names, and is where the one other
/// real-time desktop encoder to consult sits. Measured rather than inherited: see
/// `video::measure_the_encoders`, which sweeps it.
const CPU_USED: c_int = 7;

/// The frame rate the level calculation assumes.
///
/// Nothing paces frames to it — the remote decides when a frame happens — but a VP9 level is
/// defined over a *sample rate*, so a number is needed to turn a picture size into one. 30 is
/// what `VIDEO_FRAME_INTERVAL` in [`crate::encode`] paces access units to, so it is the rate a
/// stream cannot exceed rather than a guess.
const NOMINAL_FPS: u64 = 30;

/// The 1–100 quality dial as a VP9 quantizer.
///
/// The two scales run opposite ways: [`QUALITY_MIN`] is the coarsest picture and becomes
/// [`Q_COARSEST`], [`QUALITY_MAX`] the finest and becomes [`Q_FINEST`]. Out-of-range input is
/// clamped rather than refused — `config` is what rejects a bad dial.
fn q_for(quality: u8) -> u32 {
    let quality = u32::from(quality.clamp(QUALITY_MIN, QUALITY_MAX));
    let span = Q_COARSEST - Q_FINEST;
    Q_COARSEST - ((quality - 1) * span / 99)
}

/// VP9's levels, as `(level, max_luma_sample_rate, max_luma_picture_size, max_luma_breadth)`.
///
/// Transcribed from `vp9_level_defs[]` in libvpx's own `vp9/encoder/vp9_encoder.c` at the commit
/// `libvpx-prebuilt` pins, and not from memory: the table is the normative one, the rows are not
/// evenly spaced, and 4K30 lands on level 5.0 rather than the 5.1 a plausible guess gives. Only
/// the three fields a picture size can violate are kept — the rest of each row is about bitrate,
/// buffer size and tiling, none of which decides the level of a stream whose quantizer is pinned.
///
/// The lowest row a picture fits is the level announced, which matters because a level is a
/// ceiling: a decoder that accepts 5.0 accepts every stream below it, so announcing the smallest
/// true level is the widest claim that is honest.
const LEVELS: [(u8, u64, u32, u16); 14] = [
    (10, 829_440, 36_864, 512),
    (11, 2_764_800, 73_728, 768),
    (20, 4_608_000, 122_880, 960),
    (21, 9_216_000, 245_760, 1_344),
    (30, 20_736_000, 552_960, 2_048),
    (31, 36_864_000, 983_040, 2_752),
    (40, 83_558_400, 2_228_224, 4_160),
    (41, 160_432_128, 2_228_224, 4_160),
    (50, 311_951_360, 8_912_896, 8_384),
    (51, 588_251_136, 8_912_896, 8_384),
    (52, 1_176_502_272, 8_912_896, 8_384),
    (60, 1_176_502_272, 35_651_584, 16_832),
    (61, 2_353_004_544, 35_651_584, 16_832),
    (62, 4_706_009_088, 35_651_584, 16_832),
];

/// The WebCodecs codec string for a `w`×`h` VP9 stream: `vp09.<profile>.<level>.<depth>`.
///
/// Profile 0 and 8-bit, because that is what this encoder produces — I420 chroma at eight bits —
/// and the level comes from [`LEVELS`]. `None` for a picture no VP9 level covers, which
/// [`crate::video::coded_rect`] has already refused long before this is reached; it is `Option`
/// rather than a panic because this runs on the session's own path and the whole module's premise
/// is that nothing here aborts the process.
///
/// This is what `ServerMsg::VideoFormat` carries, and it is derived here rather than in the
/// client because VP9 has no in-band parameter sets at all: there is nothing in the bitstream for
/// a client to read a codec string out of.
pub fn codec_string(w: u16, h: u16) -> Option<String> {
    let size = u32::from(w) * u32::from(h);
    let rate = u64::from(size) * NOMINAL_FPS;
    let breadth = w.max(h);
    let (level, ..) = LEVELS
        .iter()
        .find(|(_, max_rate, max_size, max_breadth)| {
            rate <= *max_rate && size <= *max_size && breadth <= *max_breadth
        })
        .copied()?;
    Some(format!("vp09.00.{level:02}.08"))
}

/// One VP9 stream over a fixed rectangle of a [`Mirror`].
///
/// The rectangle is fixed for the stream's whole life, and that is what makes an inter-frame
/// stream mean anything: every frame is expressed as a change from the last one at the same
/// place. A region that moves or grows gets a *new* stream, which is [`crate::regions`]'
/// decision to make.
pub struct Stream {
    ctx: vpx::vpx_codec_ctx_t,
    /// The configuration libvpx is running on, kept so [`Self::set_quality`] can hand back the
    /// same struct with two fields changed rather than rebuild one and risk disagreeing with the
    /// encoder about a field it never meant to touch.
    cfg: vpx::vpx_codec_enc_cfg_t,
    /// An image that *borrows* the conversion buffer's planes. Built once, its pointers replaced
    /// on every frame. Never freed: `vpx_img_wrap` allocated nothing.
    img: vpx::vpx_image_t,
    /// The region as the client knows it, and as a record header reports it.
    rect: Rect,
    /// The picture actually encoded: [`Self::rect`] grown to even sides.
    coded: Rect,
    /// The conversion in front of the encoder, reused across frames.
    yuv: I420,
    /// [`Self::coded`] cropped out of the mirror, reused for the same reason.
    scratch: Vec<u8>,
    /// The 1–100 dial in force, which [`Self::set_quality`] moves and the totals report.
    quality: u8,
    /// Whether the next frame must be one a decoder can start from.
    keyframe_owed: bool,
    /// The WebCodecs codec string for this stream's picture, computed once at construction —
    /// unlike H.264's, which has to be read out of a keyframe, because VP9 carries no parameter
    /// sets and the picture size is fixed for the stream's life.
    decode: Option<String>,
    /// Where this stream's timestamps are measured from. Real elapsed time rather than a frame
    /// counter, so a pts is a millisecond on the `g_timebase` set below.
    started: std::time::Instant,
}

impl Stream {
    /// A stream over `rect` of a mirror whose coded size is `mirror`, at `quality` (1–100).
    ///
    /// The coded rectangle, and the refusal of a picture too large for it, are
    /// [`coded_rect`]'s. VP9 does not need even sides and is held to them anyway — see the note
    /// there.
    pub fn new(rect: Rect, mirror: (u16, u16), quality: u8) -> anyhow::Result<Self> {
        let coded = coded_rect(rect, mirror)?;
        let q = q_for(quality);

        // SAFETY: `vpx_codec_vp9_cx` takes no arguments and returns a pointer to a static
        // interface descriptor compiled into the archive.
        let iface = unsafe { vpx::vpx_codec_vp9_cx() };
        anyhow::ensure!(!iface.is_null(), "this libvpx has no VP9 encoder");

        // SAFETY: zeroed before libvpx is given a pointer to it, and `vpx_codec_enc_config_default`
        // writes through that pointer rather than reading it. A failure here is returned rather
        // than ignored, so nothing below reads a half-initialised config.
        let mut cfg: vpx::vpx_codec_enc_cfg_t = unsafe { std::mem::zeroed() };
        unsafe {
            vpx!(vpx::vpx_codec_enc_config_default(iface, &mut cfg, 0), "config_default")?;
        }

        cfg.g_w = u32::from(coded.w());
        cfg.g_h = u32::from(coded.h());
        // Milliseconds, so a pts is elapsed wall-clock rather than a frame index — the same
        // choice `crate::h264` makes, and for the same reason: the remote decides when a frame
        // happens and a counter would tell the encoder they all arrived on schedule.
        cfg.g_timebase.num = 1;
        cfg.g_timebase.den = 1000;
        // **0, not libvpx's default of 25.** The default holds 25 frames inside the encoder
        // before emitting anything, which on a desktop is most of a second of latency. RustDesk
        // works around the default by flushing after every frame; setting it to zero is the same
        // thing without the second code path, and it is what makes one encode mean one packet.
        cfg.g_lag_in_frames = 0;
        cfg.g_pass = vpx::vpx_enc_pass_VPX_RC_ONE_PASS;
        // No error resilience. Nothing here is ever lost in transit — the link is TCP — so it
        // would cost compression to protect against loss that cannot happen. The same argument
        // removes the periodic keyframe below.
        cfg.g_error_resilient = 0;
        // One thread per *region* stream — several regions encode in parallel with each
        // other, which is where the cores go — and several for the one stream that covers
        // the whole mirror, which has nothing to overlap with. See `video::threads_for`.
        let threads = threads_for(coded, mirror);
        cfg.g_threads = threads as u32;
        // No fixed keyframe interval; every keyframe is one somebody asked for — a repaint, a
        // resize, a client coming back, a region that grew.
        cfg.kf_mode = vpx::vpx_kf_mode_VPX_KF_DISABLED;
        // Constant quality with the quantizer pinned top and bottom. `VPX_Q` plus the `CQ_LEVEL`
        // control below is what decides it; min == max is what makes that a guarantee rather
        // than a preference, and it is why nothing here sets a bitrate — the quantizer *is* the
        // dial and the bytes land wherever the picture puts them.
        cfg.rc_end_usage = vpx::vpx_rc_mode_VPX_Q;
        cfg.rc_min_quantizer = q;
        cfg.rc_max_quantizer = q;
        // Zero, explicitly, and this one is load-bearing rather than tidy: `crate::tiles::Shadow`
        // records source pixels as delivered the moment they are blitted into the mirror, so a
        // frame libvpx decided to drop is permanently wrong pixels that nothing re-sends. It is
        // also libvpx's default, and a default is a thing that can change.
        cfg.rc_dropframe_thresh = 0;
        // Likewise: an encoder that resized itself would change the picture size mid-stream,
        // while every stream here has one rectangle for its whole life and a client crops the
        // decoded picture to it.
        cfg.rc_resize_allowed = 0;

        // SAFETY: `ctx` is zeroed and written through by `enc_init_ver`, `cfg` is the struct
        // libvpx just filled in and this function then edited by name, and the ABI version is
        // the one the linked archive's headers declared — which is what makes a Rust-side layout
        // disagreement a named error here rather than memory corruption later.
        let mut ctx: vpx::vpx_codec_ctx_t = unsafe { std::mem::zeroed() };
        unsafe {
            vpx!(
                vpx::vpx_codec_enc_init_ver(
                    &mut ctx,
                    iface,
                    &cfg,
                    0,
                    vpx::VPX_ENCODER_ABI_VERSION as c_int,
                ),
                "enc_init_ver"
            )
            .map_err(|e| {
                anyhow::anyhow!("vp9 encoder for a {}x{} picture: {e}", coded.w(), coded.h())
            })?;
        }

        // Everything from here on has a context to destroy, so the error paths go through a
        // constructed `Self` rather than returning: `Drop` is what releases the encoder, and an
        // early `?` between `enc_init_ver` and the struct would leak it.
        let mut stream = Self {
            ctx,
            cfg,
            // SAFETY: zeroed, then filled in by `vpx_img_wrap` below.
            img: unsafe { std::mem::zeroed() },
            rect,
            coded,
            // Even by construction — `coded_rect`'s theorem — which is what keeps the conversion
            // from asserting on an odd chroma plane.
            yuv: I420::new(coded.w(), coded.h()),
            scratch: Vec::new(),
            quality: quality.clamp(QUALITY_MIN, QUALITY_MAX),
            keyframe_owed: false,
            decode: codec_string(coded.w(), coded.h()),
            started: std::time::Instant::now(),
        };

        // SAFETY: `ctx` is live and each control's argument really is an `int` — the one thing
        // `vpx_codec_control_`'s variadic signature cannot check.
        unsafe {
            // In `VPX_Q` mode this is the value that actually decides the quantizer. A *control*,
            // not a config field, which is the easiest thing about libvpx's rate control to get
            // wrong: setting only `rc_min/max_quantizer` leaves `cq_level` at its default and the
            // dial half-connected.
            stream.control(vpx::vp8e_enc_control_id_VP8E_SET_CQ_LEVEL, q as c_int, "cq_level")?;
            // What this encoder is actually looking at. Screen content is mostly flat colour,
            // hard edges and text, none of which a camera preset expects.
            stream.control(
                vpx::vp8e_enc_control_id_VP9E_SET_TUNE_CONTENT,
                vpx::vp9e_tune_content_VP9E_CONTENT_SCREEN as c_int,
                "tune_content",
            )?;
            stream.control(vpx::vp8e_enc_control_id_VP8E_SET_CPUUSED, CPU_USED, "cpuused")?;
            // Off: adaptive quantization would move the quantizer off the dial that was just
            // pinned, which is the same reason `crate::h264` turns openh264's off.
            stream.control(vpx::vp8e_enc_control_id_VP9E_SET_AQ_MODE, 0, "aq_mode")?;
            if threads > 1 {
                // Both are what turns `g_threads` into actual parallelism on one picture:
                // row-based multithreading inside a tile, and enough tile columns for the
                // threads to have separate work. Inert at one thread, so they are set only
                // where the whole desktop is one stream. libvpx clamps the column count to
                // what the picture's breadth allows.
                stream.control(vpx::vp8e_enc_control_id_VP9E_SET_ROW_MT, 1, "row_mt")?;
                stream.control(
                    vpx::vp8e_enc_control_id_VP9E_SET_TILE_COLUMNS,
                    threads.ilog2() as c_int,
                    "tile_columns",
                )?;
            }

            // An image that *wraps* the conversion buffer rather than owning one. A non-null
            // pointer that is never dereferenced is libvpx's own idiom for "compute the layout,
            // allocate nothing" — ffmpeg passes a literal `1` — and it is why `vpx_img_free` must
            // never be called on this image: the planes it ends up pointing at belong to `yuv`.
            let wrapped = vpx::vpx_img_wrap(
                &mut stream.img,
                vpx::vpx_img_fmt_VPX_IMG_FMT_I420,
                cfg.g_w,
                cfg.g_h,
                1,
                std::ptr::dangling_mut::<u8>(),
            );
            anyhow::ensure!(!wrapped.is_null(), "vpx_img_wrap refused a {}x{} picture", cfg.g_w, cfg.g_h);
        }

        Ok(stream)
    }

    /// The region this stream is for — what a record header reports, and what a client crops the
    /// decoded picture to.
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// The dial this stream is currently encoding at.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// The WebCodecs codec string for this stream. Known from construction, unlike H.264's.
    pub fn decode_string(&self) -> Option<&str> {
        self.decode.as_deref()
    }

    /// Make the next access unit one a decoder can start from.
    ///
    /// Recorded rather than done, because libvpx takes it as a flag on the next `encode` call —
    /// there is nothing to tell the encoder in advance.
    pub fn force_keyframe(&mut self) {
        self.keyframe_owed = true;
    }

    /// Move the dial on the live encoder, without a keyframe.
    ///
    /// This is how a congested link gives up quality (see `Congestion` in [`crate::encode`]), and
    /// "without a keyframe" is the whole reason it is written this way rather than by rebuilding
    /// the encoder — which would force a keyframe on the next frame, spending a few hundred KB at
    /// the exact moment the link has run out of room.
    ///
    /// `vpx_codec_enc_config_set` is libvpx's own mechanism for it, and both halves are needed:
    /// the config carries the quantizer bounds and the control carries the value `VPX_Q` mode
    /// actually reads.
    pub fn set_quality(&mut self, quality: u8) -> anyhow::Result<()> {
        let quality = quality.clamp(QUALITY_MIN, QUALITY_MAX);
        let q = q_for(quality);
        self.quality = quality;
        if q == self.cfg.rc_min_quantizer {
            return Ok(());
        }
        self.cfg.rc_min_quantizer = q;
        self.cfg.rc_max_quantizer = q;
        // SAFETY: `cfg` is the struct libvpx validated at init with two fields changed, and `ctx`
        // is live. libvpx re-validates it and returns a code rather than accepting nonsense.
        unsafe {
            vpx!(vpx::vpx_codec_enc_config_set(&mut self.ctx, &self.cfg), "enc_config_set")?;
            self.control(vpx::vp8e_enc_control_id_VP8E_SET_CQ_LEVEL, q as c_int, "cq_level")?;
        }
        Ok(())
    }

    /// Encode this stream's rectangle of `mirror` as it stands.
    ///
    /// `None` means the encoder produced no bitstream. The caller must then leave its dirty flag
    /// set, so those pixels ride on the next frame — which is what keeps a frame that produced
    /// nothing from becoming pixels the client never gets. With `rc_dropframe_thresh = 0` and
    /// `g_lag_in_frames = 0` it should be unreachable; it is a return value rather than an
    /// assertion because the caller already has to handle it for the other codec.
    ///
    /// The mirror must have been padded ([`Mirror::pad_edges`]) if any stream reaches into the
    /// padding, which is the caller's job because it is once per round rather than once per
    /// stream.
    pub fn encode(
        &mut self,
        mirror: &Mirror,
        mark: Option<Mark>,
    ) -> anyhow::Result<Option<AccessUnit>> {
        let coded = (usize::from(self.coded.w()), usize::from(self.coded.h()));
        // The whole-mirror stream reads the mirror's own buffer; only a sub-rectangle,
        // or a debug outline that must not be painted on the source, goes through the
        // crop. See `Mirror::whole`.
        let rgb: &[u8] = match (mark, mirror.whole(self.coded)) {
            (None, Some(rgb)) => rgb,
            _ => {
                mirror.crop_into(self.coded, &mut self.scratch)?;
                if let Some(mark) = mark {
                    outline(&mut self.scratch, coded, mark);
                }
                &self.scratch
            }
        };
        self.yuv.read_rgb(rgb)?;

        let (y, u, v) = self.yuv.planes();
        let (sy, su, sv) = self.yuv.strides();
        let flags = if self.keyframe_owed { i64::from(vpx::VPX_EFLAG_FORCE_KF) } else { 0 };
        let pts = self.started.elapsed().as_millis() as i64;

        // SAFETY: the three planes outlive this call — they belong to `self.yuv`, which nothing
        // touches until the next `encode` — and the strides are the ones the conversion reports
        // for exactly this picture. Casting away const is what the C API requires; libvpx does not
        // write to an input image. The packets drained below point into the encoder and are copied
        // before it is called again, which is the lifetime libvpx documents for them.
        let unit = unsafe {
            self.img.planes[0] = y.as_ptr().cast_mut();
            self.img.planes[1] = u.as_ptr().cast_mut();
            self.img.planes[2] = v.as_ptr().cast_mut();
            self.img.stride[0] = sy as c_int;
            self.img.stride[1] = su as c_int;
            self.img.stride[2] = sv as c_int;

            vpx!(
                vpx::vpx_codec_encode(
                    &mut self.ctx,
                    &self.img,
                    pts,
                    // Duration: one tick of `g_timebase`, so one millisecond — deliberately a
                    // constant rather than the gap since the last frame. libvpx reads it for
                    // rate control, and there is no rate control here to read it: `VPX_Q` with
                    // `rc_min == rc_max` pins the quantizer, no bitrate is set, and
                    // `rc_dropframe_thresh = 0` forbids the one decision a duration could
                    // otherwise drive. What must stay true is `pts`, which is real elapsed
                    // time — a decoder and a `VideoDecoder` timestamp read that, not this.
                    1,
                    flags,
                    vpx::VPX_DL_REALTIME as c_ulong,
                ),
                "encode"
            )?;

            let mut data: Vec<u8> = Vec::new();
            let mut keyframe = false;
            let mut packets = 0usize;
            let mut iter: vpx::vpx_codec_iter_t = std::ptr::null();
            loop {
                let pkt = vpx::vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind != vpx::vpx_codec_cx_pkt_kind_VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let frame = &(*pkt).data.frame;
                data.extend_from_slice(std::slice::from_raw_parts(frame.buf.cast::<u8>(), frame.sz));
                keyframe |= frame.flags & vpx::VPX_FRAME_IS_KEY != 0;
                packets += 1;
            }
            // More than one packet is a superframe, which is a legal thing to concatenate and
            // hand to a decoder as one unit — but it also means `g_lag_in_frames = 0` is not doing
            // what this module assumes, so it is logged rather than silently absorbed.
            if packets > 1 {
                log::warn!("vp9: one encode produced {packets} packets; expected one");
            }
            if data.is_empty() { None } else { Some(AccessUnit { data, keyframe }) }
        };

        // Cleared only on success, and only when something came out: a frame that produced no
        // bitstream still owes its keyframe, and the caller's dirty flag is what brings it back.
        if unit.is_some() {
            self.keyframe_owed = false;
        }
        Ok(unit)
    }

    /// One `vpx_codec_control_` call with an `int` argument, checked.
    ///
    /// # Safety
    ///
    /// `id` must be a control whose argument really is an `int`. The call is variadic, so passing
    /// the wrong type compiles and corrupts the stack.
    unsafe fn control(
        &mut self,
        id: vpx::vp8e_enc_control_id,
        value: c_int,
        what: &str,
    ) -> anyhow::Result<()> {
        unsafe { vpx!(vpx::vpx_codec_control_(&mut self.ctx, id as c_int, value), what) }
    }
}

// SAFETY: a `Stream` owns its encoder exclusively — it is not `Clone`, `encode` and `set_quality`
// take `&mut self`, and libvpx keeps no thread-local state for an encoder instance, so moving one
// between threads is sound. This is the same claim openh264's crate makes for its own `Encoder`,
// and it is needed for the same reason: [`crate::regions::Round`] carries every live stream onto a
// `spawn_blocking` worker for the encode and back afterwards.
//
// Deliberately **not** `Sync`. Two threads calling `vpx_codec_encode` on one context concurrently
// is undefined behaviour, and nothing here needs to: the round holds the streams exclusively for
// as long as it is encoding them.
//
// `img`'s plane pointers borrow `yuv`, which moves with the struct — they are overwritten at the
// top of every `encode` before libvpx reads them, so a stale pointer is never dereferenced.
unsafe impl Send for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: destroyed exactly once — `Stream` is not `Clone` and holds the only handle. The
        // wrapped image is deliberately *not* freed: `vpx_img_wrap` allocated nothing, and its
        // planes belong to `self.yuv`.
        unsafe {
            vpx::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle from a position and a size, which is what most of these want.
    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// `w`×`h` of one colour.
    fn flat(w: u16, h: u16, colour: [u8; 3]) -> Vec<u8> {
        colour
            .iter()
            .copied()
            .cycle()
            .take(usize::from(w) * usize::from(h) * 3)
            .collect()
    }

    /// A mirror and one stream over the whole of it, which is the `video` shape.
    fn whole(w: u16, h: u16, quality: u8) -> (Mirror, Stream) {
        let mirror = Mirror::new(w, h).expect("a mirror");
        let stream = Stream::new(mirror.rect(), mirror.coded(), quality).expect("a stream");
        (mirror, stream)
    }

    /// Blit a moving block and encode, so there is something for the quantizer to be coarse
    /// about — a stream of identical frames costs nearly nothing at any quality and would
    /// compare two zeroes.
    fn moving(mirror: &mut Mirror, stream: &mut Stream, step: u16) -> AccessUnit {
        let mut picture = flat(320, 240, [30, 60, 90]);
        let stride = 320 * 3;
        for row in 0..80 {
            let at = (usize::from(step) * 3 + row) * stride + usize::from(step) * 9;
            picture[at..at + 300].fill(230);
        }
        mirror.blit(rect(0, 0, 320, 240), &picture).expect("a full-screen blit");
        stream.encode(mirror, None).expect("an encode").expect("an access unit")
    }

    #[test]
    fn the_quality_dial_maps_onto_the_encoders_whole_range() {
        assert_eq!(q_for(QUALITY_MIN), Q_COARSEST);
        assert_eq!(q_for(QUALITY_MAX), Q_FINEST);
        for quality in 0..=u8::MAX {
            let q = q_for(quality);
            assert!(
                (Q_FINEST..=Q_COARSEST).contains(&q),
                "quality {quality} gave q {q}, outside VP9's range"
            );
        }
        // Monotone: a higher dial is never a coarser picture.
        for quality in 1..100u8 {
            assert!(q_for(quality) >= q_for(quality + 1));
        }
    }

    #[test]
    fn a_lower_quality_makes_a_smaller_stream() {
        let bytes = |quality| {
            let (mut mirror, mut stream) = whole(320, 240, quality);
            (0..5u16).map(|step| moving(&mut mirror, &mut stream, step).data.len()).sum::<usize>()
        };
        let (coarse, fine) = (bytes(5), bytes(90));
        assert!(
            coarse < fine,
            "quality 5 encoded {coarse} bytes and quality 90 encoded {fine}; the dial is not \
             reaching the encoder"
        );
    }

    #[test]
    fn the_first_frame_is_a_keyframe_and_another_can_be_asked_for() {
        let (mut mirror, mut stream) = whole(320, 240, 60);
        assert!(
            moving(&mut mirror, &mut stream, 0).keyframe,
            "a decoder has to be able to start somewhere"
        );
        assert!(!moving(&mut mirror, &mut stream, 1).keyframe, "an unasked-for keyframe is bytes for nothing");
        assert!(!moving(&mut mirror, &mut stream, 2).keyframe);
        stream.force_keyframe();
        assert!(
            moving(&mut mirror, &mut stream, 3).keyframe,
            "force_keyframe did not reach the encoder"
        );
        // And it is not sticky: the frame after a forced keyframe is an ordinary one.
        assert!(!moving(&mut mirror, &mut stream, 4).keyframe);
    }

    /// The mechanism the congestion loop rests on: quality can be given up mid-stream without
    /// spending a keyframe to do it.
    #[test]
    fn the_quality_moves_on_a_live_encoder_without_a_keyframe() {
        let (mut mirror, mut stream) = whole(320, 240, 90);
        assert_eq!(stream.quality(), 90);

        moving(&mut mirror, &mut stream, 0);
        let fine: usize = (1..5).map(|step| moving(&mut mirror, &mut stream, step).data.len()).sum();

        stream.set_quality(QUALITY_MIN).expect("the encoder to accept a new quantizer");
        assert_eq!(stream.quality(), QUALITY_MIN);
        let coarse: Vec<_> = (5..9).map(|step| moving(&mut mirror, &mut stream, step)).collect();

        assert!(
            coarse.iter().map(|unit| unit.data.len()).sum::<usize>() < fine,
            "the quantizer did not reach the running encoder"
        );
        assert!(
            !coarse.iter().any(|unit| unit.keyframe),
            "moving the quantizer cost a keyframe, which is what this avoids"
        );
    }

    /// The odd case, which is where a chroma plane would be half a pixel wide if
    /// [`coded_rect`]'s theorem did not hold.
    #[test]
    fn an_odd_desktop_is_padded_and_still_encodes() {
        let (mut mirror, mut stream) = whole(1919, 1079, 60);
        assert_eq!(stream.rect(), mirror.rect(), "a record header carries the true region");
        mirror
            .blit(rect(0, 0, 1919, 1079), &flat(1919, 1079, [90, 90, 90]))
            .expect("a full-screen blit");
        mirror.pad_edges();
        assert!(stream.encode(&mirror, None).expect("an encode").is_some());
    }

    /// The level table, at the rows a desktop actually lands on. The numbers are libvpx's own;
    /// what is tested is that the *lowest* fitting row is chosen, since a level is a ceiling and
    /// announcing a higher one narrows the set of decoders that will accept the stream.
    #[test]
    fn the_codec_string_names_the_lowest_level_that_fits() {
        // 1280x800 at 30: 1_024_000 samples. Level 3.1 allows only 983_040 of them, so this is
        // level 4 — the *picture size* binds here, not the sample rate, which at 30_720_000 is
        // well inside 3.1's 36_864_000. That is the trap in this table: the two limits do not
        // move together, and a level chosen from the frame rate alone is wrong for the default
        // desktop size in this gateway's own config.
        assert_eq!(codec_string(1280, 800).as_deref(), Some("vp09.00.40.08"));
        // 1920x1080: 2_073_600 samples, inside level 4's picture size of 2_228_224, and
        // 62_208_000 per second inside its 83_558_400.
        assert_eq!(codec_string(1920, 1080).as_deref(), Some("vp09.00.40.08"));
        // 3840x2160: 8_294_400 samples — past 4.1's picture size, inside 5.0's 8_912_896 — and
        // 248_832_000 per second, inside 5.0's 311_951_360. Level 5.0, not the 5.1 a guess gives.
        assert_eq!(codec_string(3840, 2160).as_deref(), Some("vp09.00.50.08"));
        // Small, but not level 1: 76_800 samples is already past level 1.1's 73_728. Level 1 is
        // 256x144, which no desktop is.
        assert_eq!(codec_string(320, 240).as_deref(), Some("vp09.00.20.08"));
        // Profile and bit depth are fixed: this encoder is I420 at eight bits, whatever the size.
        for (w, h) in [(320u16, 240u16), (1920, 1080), (3840, 2160)] {
            let string = codec_string(w, h).expect("a level for a real desktop");
            assert!(string.starts_with("vp09.00."), "not profile 0: {string}");
            assert!(string.ends_with(".08"), "not 8-bit: {string}");
        }
    }

    #[test]
    fn a_picture_too_large_is_refused_by_name() {
        let Err(refused) = Stream::new(rect(0, 0, 5120, 2880), (5120, 2880), 60) else {
            panic!("a 5K picture was accepted");
        };
        let message = format!("{refused:#}");
        assert!(message.contains("5120x2880"), "the message does not say what was asked for");
        assert!(message.contains("3840"), "the message does not say what the limit is");
        assert!(message.contains("webp"), "the message does not say what to do instead");
    }

    /// Two streams over disjoint regions of one mirror, which is the `motion` shape: each sees
    /// its own pixels and neither sees the other's.
    #[test]
    fn two_regions_of_one_mirror_encode_independently() {
        let mut mirror = Mirror::new(640, 128).expect("a mirror");
        let left = Rect { left: 0, top: 0, right: 319, bottom: 127 };
        let right = Rect { left: 320, top: 0, right: 639, bottom: 127 };
        let mut a = Stream::new(left, mirror.coded(), 60).expect("a stream");
        let mut b = Stream::new(right, mirror.coded(), 60).expect("a stream");

        mirror.blit(left, &flat(320, 128, [200, 30, 30])).expect("a blit");
        mirror.blit(right, &flat(320, 128, [30, 30, 200])).expect("a blit");
        assert!(a.encode(&mirror, None).expect("an encode").expect("a unit").keyframe);
        assert!(b.encode(&mirror, None).expect("an encode").expect("a unit").keyframe);

        // Only the left region changes. The right one still encodes — nothing here decides
        // whether it should, that is the caller's dirty flag — but it has nothing to describe.
        mirror.blit(left, &flat(320, 128, [30, 200, 30])).expect("a blit");
        let changed = a.encode(&mirror, None).expect("an encode").expect("a unit").data.len();
        let unchanged = b.encode(&mirror, None).expect("an encode").expect("a unit").data.len();
        assert!(
            unchanged < changed,
            "the right region cost {unchanged} bytes against the changed one's {changed}; the \
             two streams are not seeing different pixels"
        );
    }
}
