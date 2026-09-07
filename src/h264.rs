//! H.264 encoding: one stream over a rectangle of a [`crate::video::Mirror`], through
//! Cisco's openh264.
//!
//! The fallback video codec. [`crate::vp9`] is the codec; this module exists for the
//! browser whose `VideoDecoder` has no VP9 — older Safari on hardware without a VP9
//! block — and is used for a session only when that browser asks
//! ([`crate::config::VideoCodec`]). It presents the same surface as the VP9 stream so
//! that [`crate::video::Stream`] can hold either and [`crate::regions`] never knows which:
//! the same rectangle rule, the same 1–100 dial, the same "no frame is ever dropped",
//! the same keyframe-on-demand, and a quality that moves on the live encoder without
//! spending a keyframe. One keyframe is openh264's own rather than the gateway's: its
//! screen-content mode insists on scene-change detection, so a change large enough to
//! read as a cut — a whole-desktop repaint — is an IDR whether or not one was asked
//! for. The keyframe bit on the wire comes from the encoder either way.
//!
//! What differs is named where it differs. 4:2:0 only — openh264 has no 4:4:4 profile,
//! so a `render_chroma = "444"` target streams 4:2:0 to a fallback browser
//! ([`crate::regions::Regions::new`] says so in the log). The bitstream is Annex B with
//! the parameter sets in band on every IDR, which is the WebCodecs form that needs no
//! `description`: the configuration string is still the whole of what
//! `ServerMsg::VideoFormat` carries.
//!
//! openh264's C API is a COM-style vtable behind `WelsCreateSVCEncoder`, driven here
//! directly through `openh264-sys2` rather than the `openh264` crate's `Encoder`: that
//! wrapper owns its parameters, reads the quantizer from a range only a rate controller
//! clips to — the mode used here has none, and the dial has to land on `iDLayerQp`,
//! which it never sets — and keeps the initialisation call private. Two things about
//! the API's shape are worth knowing before reading: it returns `CM_RETURN` codes with
//! no string table behind them, so every call goes through [`checked`] and a failure
//! ends one session with the code in the message; and every vtable entry is an
//! `Option<fn>`, resolved once at construction so no call site unwraps.

use std::os::raw::{c_int, c_void};

use openh264_sys2 as sys;
use openh264_sys2::source::APILoader;

use crate::config::Chroma;
use crate::tiles::Rect;
use crate::video::{
    AccessUnit, Mark, Mirror, QUALITY_MAX, QUALITY_MIN, Yuv, coded_rect, outline, threads_for,
};

/// Turn an openh264 return code into an `anyhow::Error` naming the call.
///
/// openh264 speaks `CM_RETURN` codes and nothing else — there is no string table to
/// consult, so the number is the whole explanation.
fn checked(code: c_int, what: &str) -> anyhow::Result<()> {
    if code == sys::cmResultSuccess {
        Ok(())
    } else {
        Err(anyhow::anyhow!("h264 {what}: openh264 returned {code}"))
    }
}

/// The quantizer the dial spans. H.264's QP is 0–51, coarsest last, and every six steps
/// doubles the quantization step.
///
/// The fine end is 12 rather than 0 for the reason VP9's is 8 (`vp9::Q_FINEST`): below
/// it screen content is already visually lossless and the bytes keep doubling, so a dial
/// that reached lower would have a top stretch where turning it bought nothing but
/// bandwidth. Settled with `video::measure_the_encoder`, which sweeps both codecs.
const QP_FINEST: u8 = 12;
/// See [`QP_FINEST`]. 51 is H.264's coarsest.
const QP_COARSEST: u8 = 51;

/// The frame rate the level calculation assumes and the encoder is told.
///
/// Nothing paces frames to it — the remote decides when a frame happens — but an H.264
/// level is a ceiling on *macroblocks per second*, so a rate is needed to turn a picture
/// size into one, and openh264 wants the same number to size its own bookkeeping. 30 is
/// what `VIDEO_FRAME_INTERVAL` in [`crate::encode`] paces access units to: the rate a
/// stream cannot exceed rather than a guess.
const NOMINAL_FPS: u32 = 30;

/// H.264's levels, as `(level_idc, max_mb_per_second, max_frame_size_in_mbs)`.
///
/// Table A-1 of the specification, the two columns a picture size can violate — the
/// rest of each row is bitrate and buffer, which decides nothing for a stream whose
/// quantizer is pinned. Level 1b is left out: it shares 1.0's limits and no desktop is
/// that small. `NOMINAL_FPS` supplies the rate, and the lowest row a picture fits is the
/// level announced, because a level is a ceiling: a decoder that accepts 5.1 accepts
/// every stream below it, so the smallest true level is the widest honest claim.
///
/// The last row is the encoder's own ceiling too: openh264 stops at 5.2, and 3840×2400
/// at 30 — the largest picture `video::MAX_LONG_SIDE` admits — needs exactly that.
const LEVELS: [(u8, u32, u32); 16] = [
    (10, 1_485, 99),
    (11, 3_000, 396),
    (12, 6_000, 396),
    (13, 11_880, 396),
    (20, 11_880, 396),
    (21, 19_800, 792),
    (22, 20_250, 1_620),
    (30, 40_500, 1_620),
    (31, 108_000, 3_600),
    (32, 216_000, 5_120),
    (40, 245_760, 8_192),
    (41, 245_760, 8_192),
    (42, 522_240, 8_704),
    (50, 589_824, 22_080),
    (51, 983_040, 36_864),
    (52, 2_073_600, 36_864),
];

/// The 1–100 quality dial as an H.264 QP.
///
/// The two scales run opposite ways: [`QUALITY_MIN`] is the coarsest picture and becomes
/// [`QP_COARSEST`], [`QUALITY_MAX`] the finest and becomes [`QP_FINEST`]. Out-of-range
/// input is clamped rather than refused — `config` is what rejects a bad dial.
fn qp_for(quality: u8) -> u8 {
    let quality = u32::from(quality.clamp(QUALITY_MIN, QUALITY_MAX));
    let span = u32::from(QP_COARSEST - QP_FINEST);
    (u32::from(QP_COARSEST) - ((quality - 1) * span / 99)) as u8
}

/// The level a `w`×`h` picture needs at [`NOMINAL_FPS`], as the `level_idc` the SPS
/// carries. `None` for a picture no level covers, which [`coded_rect`] has already
/// refused before this is reached.
fn level_for(w: u16, h: u16) -> Option<u8> {
    let mbs = u32::from(w.div_ceil(16)) * u32::from(h.div_ceil(16));
    let rate = mbs * NOMINAL_FPS;
    LEVELS
        .iter()
        .find(|(_, max_rate, max_size)| rate <= *max_rate && mbs <= *max_size)
        .map(|(level, ..)| *level)
}

/// The WebCodecs codec string for a `w`×`h` H.264 stream: `avc1.PPCCLL`, the profile,
/// constraint flags and level as the SPS will carry them, in hex.
///
/// High profile (`64`) with no constraint flags: what the encoder is configured for below
/// — CABAC and the 8×8 transform — and what every hardware decoder of the last fifteen
/// years takes, the ones in the browsers this codec exists for included. The level is the
/// picture's, from `LEVELS`, and the encoder is told the same number so the SPS and the
/// string agree. Unlike VP9's string this one carries no colour fields: H.264 declares
/// its colour in the SPS's VUI, which is set below and read from the bitstream.
///
/// Derived here rather than parsed out of the first keyframe because
/// `ServerMsg::VideoFormat` has to be announced before the stream's first unit exists.
pub fn codec_string(w: u16, h: u16) -> Option<String> {
    level_for(w, h).map(|level| format!("avc1.6400{level:02X}"))
}

/// One H.264 stream over a fixed rectangle of a [`Mirror`].
///
/// The rectangle is fixed for the stream's whole life — the same contract as the VP9
/// stream's, and for the same reason: every frame is a change from the last one at the
/// same place, and a region that moves or grows is a new stream.
pub struct Stream {
    /// The encoder instance: a pointer to a pointer to its vtable, as openh264's C API
    /// shapes it. Created by `WelsCreateSVCEncoder`, released by `Drop`.
    encoder: *mut sys::ISVCEncoder,
    /// The vtable's entries, resolved once. See [`Vtable`].
    vtable: Vtable,
    /// The region as the client knows it, and as a record header reports it.
    rect: Rect,
    /// The picture actually encoded: [`Self::rect`] grown to even sides.
    coded: Rect,
    /// The conversion in front of the encoder, reused across frames. Always 4:2:0.
    yuv: Yuv,
    /// [`Self::coded`] cropped out of the mirror, reused for the same reason.
    scratch: Vec<u8>,
    /// The 1–100 dial in force, which [`Self::set_quality`] moves and the totals report.
    quality: u8,
    /// Whether the next frame must be one a decoder can start from.
    keyframe_owed: bool,
    /// The WebCodecs codec string for this stream's picture, computed once at
    /// construction: the level follows from the picture size, which is fixed for the
    /// stream's life.
    decode: Option<String>,
    /// Where this stream's timestamps are measured from — real elapsed time, so a
    /// timestamp is a millisecond and not a frame count.
    started: std::time::Instant,
}

impl Stream {
    /// A stream over `rect` of a mirror whose coded size is `mirror`, at `quality` (1–100).
    ///
    /// No chroma parameter: this encoder is 4:2:0 and the caller has already said so in
    /// the log if the target asked for more. The coded rectangle, and the refusal of a
    /// picture too large for it, are [`coded_rect`]'s.
    pub fn new(rect: Rect, mirror: (u16, u16), quality: u8) -> anyhow::Result<Self> {
        let coded = coded_rect(rect, mirror)?;
        let quality = quality.clamp(QUALITY_MIN, QUALITY_MAX);
        let qp = qp_for(quality);
        let level = level_for(coded.w(), coded.h()).ok_or_else(|| {
            anyhow::anyhow!("h264: no level covers a {}x{} picture", coded.w(), coded.h())
        })?;
        let threads = threads_for(coded, mirror);

        // SAFETY: `WelsCreateSVCEncoder` writes a valid instance through the pointer or
        // returns a code, and the vtable it points at is static for the instance's life.
        let mut encoder: *mut sys::ISVCEncoder = std::ptr::null_mut();
        unsafe {
            checked(APILoader::WelsCreateSVCEncoder(&raw mut encoder), "create")?;
        }
        anyhow::ensure!(!encoder.is_null(), "h264: WelsCreateSVCEncoder returned no encoder");
        // Everything from here on has an instance to destroy, so the error paths go
        // through `Guard` rather than returning: an early `?` between the create and the
        // struct would leak it.
        let guard = Guard(encoder);
        // SAFETY: `encoder` is the live instance just created.
        let vtable = unsafe { Vtable::resolve(encoder)? };

        // SAFETY: `params` is a zeroed struct openh264 fills through the pointer before
        // anything reads it, and every later call passes a pointer to a live value of the
        // type the option id names.
        unsafe {
            // Quiet before anything else: the trace level is one of the three options
            // openh264 takes before initialisation, and the validation inside
            // `initialize_ext` is where its warnings come from.
            let mut trace: c_int = sys::WELS_LOG_QUIET as c_int;
            checked(
                (vtable.set_option)(
                    encoder,
                    sys::ENCODER_OPTION_TRACE_LEVEL,
                    (&raw mut trace).cast::<c_void>(),
                ),
                "trace_level",
            )?;
            let mut params = sys::SEncParamExt::default();
            checked((vtable.get_default_params)(encoder, &raw mut params), "get_default_params")?;

            params.iUsageType = sys::SCREEN_CONTENT_REAL_TIME;
            params.iPicWidth = c_int::from(coded.w());
            params.iPicHeight = c_int::from(coded.h());
            params.fMaxFrameRate = NOMINAL_FPS as f32;
            // No rate control at all: the quantizer *is* the dial, set on the layer below,
            // and the bytes land wherever the picture puts them. This is the mode in which
            // openh264 reads `iDLayerQp` and nothing else decides a QP.
            params.iRCMode = sys::RC_OFF_MODE;
            params.iTargetBitrate = sys::UNSPECIFIED_BIT_RATE as c_int;
            params.iMaxBitrate = sys::UNSPECIFIED_BIT_RATE as c_int;
            // Load-bearing, as `rc_dropframe_thresh = 0` is for VP9: `crate::tiles::Shadow`
            // records source pixels as delivered when they reach the mirror, so a frame the
            // encoder skipped is permanently wrong pixels.
            params.bEnableFrameSkip = false;
            params.iMinQp = c_int::from(qp);
            params.iMaxQp = c_int::from(qp);
            // No periodic IDR: a keyframe is one somebody asked for, or one openh264's
            // scene-change detector decided on. That detector is not optional here —
            // `ParamValidationExt` turns it back on under screen-content usage and logs a
            // warning for having had to — so it is set rather than merely left, and the
            // module doc says what it costs.
            params.uiIntraPeriod = 0;
            params.bEnableSceneChangeDetect = true;
            // Both would move the quantizer off the dial that was just pinned.
            params.bEnableAdaptiveQuant = false;
            params.bEnableBackgroundDetection = false;
            params.bEnableLongTermReference = false;
            params.bEnableDenoise = false;
            // Nothing here is ever lost in transit — the link is TCP.
            params.bIsLosslessLink = true;
            // Medium, by measurement rather than default: `video::measure_the_encoder`
            // on 2026-09-07 put low complexity at 21–23 ms per 1080p frame against
            // medium's 20–21, and at 1280×800 10.6–11.1 against 9.9–10.4 — no faster,
            // for a worse picture. Either is about twice libvpx's time on the same
            // pixels, still inside the 33 ms frame interval.
            params.iComplexityMode = sys::MEDIUM_COMPLEXITY;
            params.iSpatialLayerNum = 1;
            params.iTemporalLayerNum = 1;
            // One SPS/PPS id for the stream's life. Every IDR carries them in band, which
            // is what lets an Annex B `VideoDecoder` start from any keyframe with no
            // `description` — a client that came back mid-stream reads them off the
            // keyframe the repaint forced.
            params.eSpsPpsIdStrategy = sys::CONSTANT_ID;
            params.bPrefixNalAddingCtrl = false;
            // High profile: CABAC and the 8×8 transform. The profile is set on the layer
            // and the entropy flag here, and openh264 refuses the pair any other way round.
            params.iEntropyCodingModeFlag = 1;
            // Several threads only for the one stream that covers the whole mirror —
            // `threads_for` — and openh264's threading is per slice, so the picture is cut
            // into as many slices as threads. Each slice is its own NAL inside the one
            // access unit, which a decoder takes as one picture.
            params.iMultipleThreadIdc = threads as u16;

            let layer = &mut params.sSpatialLayers[0];
            layer.iVideoWidth = c_int::from(coded.w());
            layer.iVideoHeight = c_int::from(coded.h());
            layer.fFrameRate = NOMINAL_FPS as f32;
            layer.iSpatialBitrate = sys::UNSPECIFIED_BIT_RATE as c_int;
            layer.iMaxSpatialBitrate = sys::UNSPECIFIED_BIT_RATE as c_int;
            layer.uiProfileIdc = sys::PRO_HIGH;
            layer.uiLevelIdc = sys::ELevelIdc::from(level);
            // The dial, in the field `RC_OFF_MODE` reads.
            layer.iDLayerQp = c_int::from(qp);
            if threads > 1 {
                layer.sSliceArgument.uiSliceMode = sys::SM_FIXEDSLCNUM_SLICE;
                layer.sSliceArgument.uiSliceNum = threads as u32;
            } else {
                layer.sSliceArgument.uiSliceMode = sys::SM_SINGLE_SLICE;
                layer.sSliceArgument.uiSliceNum = 1;
            }
            // Say in the SPS what the conversion did: BT.601 (SMPTE 170M primaries,
            // transfer and matrix — code 6 each) at studio swing. The same declaration
            // VP9's keyframe header makes, and for the same reason: a decoder given no
            // colour description guesses, and guesses BT.709 for an HD picture.
            layer.bVideoSignalTypePresent = true;
            layer.uiVideoFormat = 5;
            layer.bFullRange = false;
            layer.bColorDescriptionPresent = true;
            layer.uiColorPrimaries = 6;
            layer.uiTransferCharacteristics = 6;
            layer.uiColorMatrix = 6;

            checked((vtable.initialize_ext)(encoder, &raw const params), "initialize_ext")
                .map_err(|e| {
                    anyhow::anyhow!("h264 encoder for a {}x{} picture: {e}", coded.w(), coded.h())
                })?;
            let mut format: sys::EVideoFormatType = sys::videoFormatI420;
            checked(
                (vtable.set_option)(
                    encoder,
                    sys::ENCODER_OPTION_DATAFORMAT,
                    (&raw mut format).cast::<c_void>(),
                ),
                "data_format",
            )?;
        }

        std::mem::forget(guard);
        Ok(Self {
            encoder,
            vtable,
            rect,
            coded,
            yuv: Yuv::new(coded.w(), coded.h(), Chroma::Subsampled),
            scratch: Vec::new(),
            quality,
            keyframe_owed: false,
            decode: codec_string(coded.w(), coded.h()),
            started: std::time::Instant::now(),
        })
    }

    /// The region this stream is for — what a record header reports, and what a client
    /// crops the decoded picture to.
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// The dial this stream is currently encoding at.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// The WebCodecs codec string for this stream, known from construction.
    pub fn decode_string(&self) -> Option<&str> {
        self.decode.as_deref()
    }

    /// Make the next access unit one a decoder can start from.
    ///
    /// Recorded rather than done: openh264's `ForceIntraFrame` is itself a flag on the
    /// next encode, and holding it here keeps the "still owed until a unit came out" rule
    /// the VP9 stream keeps.
    pub fn force_keyframe(&mut self) {
        self.keyframe_owed = true;
    }

    /// Move the dial on the live encoder, without a keyframe.
    ///
    /// Read the running parameters back, change the three quantizer fields, and hand
    /// them back. openh264 compares old and new and resets the encoder — an IDR — only
    /// for a change of picture size, threading, reference structure or profile; a
    /// quantizer is copied across in place, which is what makes this the congestion
    /// loop's move and not a restart.
    pub fn set_quality(&mut self, quality: u8) -> anyhow::Result<()> {
        let quality = quality.clamp(QUALITY_MIN, QUALITY_MAX);
        let qp = qp_for(quality);
        if qp == qp_for(self.quality) {
            self.quality = quality;
            return Ok(());
        }
        // SAFETY: the option id names a `SEncParamExt`, which is what is passed both ways;
        // openh264 copies out of and into it and keeps no pointer to it.
        unsafe {
            let mut params = sys::SEncParamExt::default();
            checked(
                (self.vtable.get_option)(
                    self.encoder,
                    sys::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                    (&raw mut params).cast(),
                ),
                "get_option(params)",
            )?;
            params.iMinQp = c_int::from(qp);
            params.iMaxQp = c_int::from(qp);
            params.sSpatialLayers[0].iDLayerQp = c_int::from(qp);
            checked(
                (self.vtable.set_option)(
                    self.encoder,
                    sys::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                    (&raw mut params).cast(),
                ),
                "set_option(params)",
            )?;
        }
        self.quality = quality;
        Ok(())
    }

    /// Encode this stream's rectangle of `mirror` as it stands.
    ///
    /// `None` means the encoder produced no bitstream, and the caller must then leave its
    /// dirty flag set so those pixels ride on the next frame. With frame skipping off it
    /// should be unreachable; it is a return value rather than an assertion because the
    /// caller already handles it for the other codec.
    ///
    /// The mirror must have been padded ([`Mirror::pad_edges`]) if any stream reaches into
    /// the padding, which is the caller's job because it is once per round rather than
    /// once per stream.
    pub fn encode(
        &mut self,
        mirror: &Mirror,
        mark: Option<Mark>,
    ) -> anyhow::Result<Option<AccessUnit>> {
        let coded = (usize::from(self.coded.w()), usize::from(self.coded.h()));
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
        let picture = sys::SSourcePicture {
            iColorFormat: sys::videoFormatI420,
            iStride: [sy as c_int, su as c_int, sv as c_int, 0],
            // Casting away const is what the C API requires; openh264 does not write to
            // an input picture.
            pData: [
                y.as_ptr().cast_mut(),
                u.as_ptr().cast_mut(),
                v.as_ptr().cast_mut(),
                std::ptr::null_mut(),
            ],
            iPicWidth: c_int::from(self.coded.w()),
            iPicHeight: c_int::from(self.coded.h()),
            uiTimeStamp: self.started.elapsed().as_millis() as i64,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        };

        // SAFETY: the three planes outlive this call — they belong to `self.yuv`, which
        // nothing touches until the next `encode` — and the strides are the conversion's
        // for exactly this picture. The bitstream openh264 hands back points into the
        // encoder and is copied out before it is called again, which is the lifetime it
        // documents. `info` is zeroed and written through by `encode_frame`.
        let unit = unsafe {
            if self.keyframe_owed {
                checked((self.vtable.force_intra_frame)(self.encoder, true), "force_intra_frame")?;
            }
            let mut info = sys::SFrameBSInfo::default();
            checked(
                (self.vtable.encode_frame)(self.encoder, &raw const picture, &raw mut info),
                "encode_frame",
            )?;
            if info.eFrameType == sys::videoFrameTypeSkip {
                None
            } else {
                let keyframe = matches!(info.eFrameType, sys::videoFrameTypeIDR | sys::videoFrameTypeI);
                let mut data = Vec::with_capacity(info.iFrameSizeInBytes.max(0) as usize);
                // One access unit is every NAL of every layer, in order — parameter sets,
                // then the slices — as Annex B already delimits them.
                for layer in &info.sLayerInfo[..info.iLayerNum.max(0) as usize] {
                    let nals = layer.iNalCount.max(0) as usize;
                    let len: usize = std::slice::from_raw_parts(layer.pNalLengthInByte, nals)
                        .iter()
                        .map(|n| n.max(&0).unsigned_abs() as usize)
                        .sum();
                    data.extend_from_slice(std::slice::from_raw_parts(layer.pBsBuf, len));
                }
                if data.is_empty() { None } else { Some(AccessUnit { data, keyframe }) }
            }
        };

        if unit.is_some() {
            self.keyframe_owed = false;
        }
        Ok(unit)
    }
}

/// The vtable entries a stream calls, resolved once from the instance's table.
///
/// openh264 declares every entry as an `Option<fn>` because the header is C; resolving
/// them here is what keeps every call site from carrying an unwrap, and a table missing
/// an entry is refused at construction by name.
#[derive(Clone, Copy)]
struct Vtable {
    initialize_ext: unsafe extern "C" fn(*mut sys::ISVCEncoder, *const sys::SEncParamExt) -> c_int,
    get_default_params: unsafe extern "C" fn(*mut sys::ISVCEncoder, *mut sys::SEncParamExt) -> c_int,
    uninitialize: unsafe extern "C" fn(*mut sys::ISVCEncoder) -> c_int,
    encode_frame: unsafe extern "C" fn(
        *mut sys::ISVCEncoder,
        *const sys::SSourcePicture,
        *mut sys::SFrameBSInfo,
    ) -> c_int,
    force_intra_frame: unsafe extern "C" fn(*mut sys::ISVCEncoder, bool) -> c_int,
    set_option: unsafe extern "C" fn(*mut sys::ISVCEncoder, sys::ENCODER_OPTION, *mut c_void) -> c_int,
    get_option: unsafe extern "C" fn(*mut sys::ISVCEncoder, sys::ENCODER_OPTION, *mut c_void) -> c_int,
}

impl Vtable {
    /// # Safety
    ///
    /// `encoder` must be a live instance from `WelsCreateSVCEncoder`.
    unsafe fn resolve(encoder: *mut sys::ISVCEncoder) -> anyhow::Result<Self> {
        // SAFETY: the caller's contract — a live instance points at its vtable.
        let table = unsafe { &**encoder };
        let entry = |name: &str| anyhow::anyhow!("h264: the openh264 vtable has no {name}");
        Ok(Self {
            initialize_ext: table.InitializeExt.ok_or_else(|| entry("InitializeExt"))?,
            get_default_params: table.GetDefaultParams.ok_or_else(|| entry("GetDefaultParams"))?,
            uninitialize: table.Uninitialize.ok_or_else(|| entry("Uninitialize"))?,
            encode_frame: table.EncodeFrame.ok_or_else(|| entry("EncodeFrame"))?,
            force_intra_frame: table.ForceIntraFrame.ok_or_else(|| entry("ForceIntraFrame"))?,
            set_option: table.SetOption.ok_or_else(|| entry("SetOption"))?,
            get_option: table.GetOption.ok_or_else(|| entry("GetOption"))?,
        })
    }
}

/// Destroys a created-but-not-yet-owned encoder if construction fails between the
/// create and the `Stream`. Forgotten on success, where `Stream`'s own `Drop` takes over.
struct Guard(*mut sys::ISVCEncoder);

impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: a created instance that no `Stream` owns; destroyed exactly once here.
        unsafe { APILoader::WelsDestroySVCEncoder(self.0) }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: destroyed exactly once — `Stream` is not `Clone` and holds the only
        // handle — and uninitialised first, which is the order openh264 documents.
        unsafe {
            (self.vtable.uninitialize)(self.encoder);
            APILoader::WelsDestroySVCEncoder(self.encoder);
        }
    }
}

// SAFETY: a `Stream` owns its encoder exclusively — it is not `Clone`, `encode` and
// `set_quality` take `&mut self` — and openh264 keeps no thread-local state for an encoder
// instance, so moving one between threads is sound. Needed because
// [`crate::regions::Round`] carries every live stream onto a `spawn_blocking` worker and
// back. Deliberately not `Sync`, for the reason the VP9 stream is not: two threads in one
// encoder at once is undefined behaviour, and nothing here needs it.
unsafe impl Send for Stream {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle from a position and a size.
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

    /// Blit a moving block and encode, so there is something for the quantizer to be
    /// coarse about.
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

    /// The NAL unit types in an Annex B access unit, in order.
    fn nal_types(data: &[u8]) -> Vec<u8> {
        openh264::nal_units(data).map(|nal| nal[nal.iter().position(|&b| b == 1).unwrap() + 1] & 0x1f).collect()
    }

    /// The other half of the library, so a claim is a round trip and not a byte count.
    /// One decoder per call: every unit these tests decode is an IDR with its parameter
    /// sets in band, which is exactly the property a fresh decoder tests.
    fn decode(unit: &AccessUnit) -> (usize, usize, Vec<u8>) {
        use openh264::formats::YUVSource as _;
        let mut decoder = openh264::decoder::Decoder::new().expect("a decoder");
        let picture = decoder
            .decode(&unit.data)
            .expect("a decode")
            .expect("the unit decoded to a picture");
        let (w, h) = picture.dimensions();
        let mut rgb = vec![0u8; w * h * 3];
        picture.write_rgb8(&mut rgb);
        (w, h, rgb)
    }

    #[test]
    fn the_quality_dial_maps_onto_the_encoders_whole_range() {
        assert_eq!(qp_for(QUALITY_MIN), QP_COARSEST);
        assert_eq!(qp_for(QUALITY_MAX), QP_FINEST);
        for quality in 0..=u8::MAX {
            let qp = qp_for(quality);
            assert!((QP_FINEST..=QP_COARSEST).contains(&qp), "quality {quality} gave QP {qp}");
        }
        for quality in 1..100u8 {
            assert!(qp_for(quality) >= qp_for(quality + 1), "a higher dial is never coarser");
        }
    }

    #[test]
    fn a_lower_quality_makes_a_smaller_stream() {
        let bytes = |quality| {
            let (mut mirror, mut stream) = whole(320, 240, quality);
            (0..5u16).map(|step| moving(&mut mirror, &mut stream, step).data.len()).sum::<usize>()
        };
        let (coarse, fine) = (bytes(5), bytes(90));
        assert!(coarse < fine, "quality 5 encoded {coarse} bytes and quality 90 encoded {fine}");
    }

    #[test]
    fn the_first_frame_is_a_keyframe_and_another_can_be_asked_for() {
        let (mut mirror, mut stream) = whole(320, 240, 60);
        assert!(moving(&mut mirror, &mut stream, 0).keyframe, "a decoder has to start somewhere");
        assert!(!moving(&mut mirror, &mut stream, 1).keyframe, "an unasked-for keyframe is bytes for nothing");
        assert!(!moving(&mut mirror, &mut stream, 2).keyframe);
        stream.force_keyframe();
        assert!(moving(&mut mirror, &mut stream, 3).keyframe, "force_keyframe did not reach the encoder");
        assert!(!moving(&mut mirror, &mut stream, 4).keyframe, "not sticky");
    }

    /// The mechanism the congestion loop rests on, and the one property of openh264's
    /// parameter update this module depends on: a quantizer change is not a reset.
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
        assert!(!coarse.iter().any(|unit| unit.keyframe), "moving the quantizer cost a keyframe");
    }

    /// Every keyframe carries its parameter sets, in Annex B order, which is what lets a
    /// `VideoDecoder` configured with nothing but the codec string start from any of them.
    #[test]
    fn a_keyframe_is_annex_b_with_its_parameter_sets_in_band() {
        let (mut mirror, mut stream) = whole(320, 240, 60);
        let idr = moving(&mut mirror, &mut stream, 0);
        assert!(idr.data.starts_with(&[0, 0, 0, 1]), "not Annex B");
        let types = nal_types(&idr.data);
        assert_eq!(&types[..2], &[7, 8], "SPS then PPS lead the keyframe: {types:?}");
        assert!(types[2..].iter().all(|&t| t == 5), "then IDR slices only: {types:?}");
        let sps = openh264::nal_units(&idr.data).next().unwrap();
        let profile_idc = sps[sps.iter().position(|&b| b == 1).unwrap() + 2];
        assert_eq!(profile_idc, 100, "not High profile — the codec string promised 0x64");

        let delta = moving(&mut mirror, &mut stream, 1);
        let types = nal_types(&delta.data);
        assert!(types.iter().all(|&t| t == 1), "an inter frame is slices only: {types:?}");
        stream.force_keyframe();
        let again = moving(&mut mirror, &mut stream, 2);
        assert_eq!(&nal_types(&again.data)[..2], &[7, 8], "a forced keyframe repeats the sets");
    }

    #[test]
    fn a_keyframe_decodes_to_the_picture_that_was_encoded() {
        let mut mirror = Mirror::new(64, 64).expect("a mirror");
        let mut stream = Stream::new(mirror.rect(), mirror.coded(), QUALITY_MAX).expect("a stream");
        mirror.blit(rect(0, 0, 64, 64), &flat(64, 64, [200, 30, 30])).expect("a blit");
        let unit = stream.encode(&mirror, None).expect("an encode").expect("a unit");
        let (w, h, rgb) = decode(&unit);
        assert_eq!((w, h), (64, 64));
        let worst = rgb
            .as_chunks::<3>()
            .0
            .iter()
            .map(|px| px.iter().zip([200u8, 30, 30]).map(|(a, b)| a.abs_diff(b)).max().unwrap())
            .max()
            .unwrap();
        assert!(worst <= 8, "a flat red came back {worst} code values off at the finest quantizer");
    }

    #[test]
    fn an_odd_desktop_is_padded_and_still_encodes() {
        let (mut mirror, mut stream) = whole(1919, 1079, 60);
        assert_eq!(stream.rect(), mirror.rect(), "a record header carries the true region");
        mirror.blit(rect(0, 0, 1919, 1079), &flat(1919, 1079, [90, 90, 90])).expect("a blit");
        mirror.pad_edges();
        assert!(stream.encode(&mirror, None).expect("an encode").is_some());
    }

    /// The ceiling `video::MAX_LONG_SIDE` admits is one openh264 takes: level 5.2 holds
    /// a 16:10 4K panel, and a whole-desktop stream over it gets its threads and slices.
    #[test]
    fn the_largest_admitted_desktop_encodes_at_level_52() {
        let (mut mirror, mut stream) = whole(3840, 2400, 40);
        assert_eq!(stream.decode_string(), Some("avc1.640034"));
        mirror.blit(rect(0, 0, 3840, 2400), &flat(3840, 2400, [40, 40, 40])).expect("a blit");
        assert!(stream.encode(&mirror, None).expect("an encode").expect("a unit").keyframe);
    }

    #[test]
    fn the_codec_string_names_the_lowest_level_that_fits() {
        // 1280x800 is 80x50 = 4000 macroblocks: past 3.1's 3600, inside 3.2's 5120.
        assert_eq!(codec_string(1280, 800).as_deref(), Some("avc1.640020"));
        // 1920x1080 is 120x68 = 8160, inside level 4's 8192 — and 244_800 per second,
        // inside its 245_760 with 960 to spare. The trap in this table.
        assert_eq!(codec_string(1920, 1080).as_deref(), Some("avc1.640028"));
        // 3840x2160 is 240x135 = 32_400: level 5.1, whose 983_040 per second holds
        // 972_000. 3840x2400 has the same frame-size level and 1_080_000 per second,
        // which only 5.2 holds.
        assert_eq!(codec_string(3840, 2160).as_deref(), Some("avc1.640033"));
        assert_eq!(codec_string(3840, 2400).as_deref(), Some("avc1.640034"));
        // 320x240 is 300 macroblocks, which level 1.1 holds — but at 9000 per second,
        // which only 1.3 holds. The rate binds where the size does not.
        assert_eq!(codec_string(320, 240).as_deref(), Some("avc1.64000D"));
        // Past every row: 4096x4096 is 65_536 macroblocks and no level holds it. The
        // ceiling in `video.rs` refuses it first; this is what the string does if asked.
        assert_eq!(codec_string(4096, 4096), None);
    }

    #[test]
    fn a_picture_too_large_is_refused_by_name() {
        let Err(refused) = Stream::new(rect(0, 0, 5120, 2880), (5120, 2880), 60) else {
            panic!("a 5K picture was accepted");
        };
        let message = format!("{refused:#}");
        assert!(message.contains("5120x2880"), "the message does not say what was asked for");
        assert!(message.contains("3840"), "the message does not say what the limit is");
    }

    /// Two streams over disjoint regions of one mirror, which is the motion shape.
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

        mirror.blit(left, &flat(320, 128, [30, 200, 30])).expect("a blit");
        let changed = a.encode(&mirror, None).expect("an encode").expect("a unit").data.len();
        let unchanged = b.encode(&mirror, None).expect("an encode").expect("a unit").data.len();
        assert!(
            unchanged < changed,
            "the right region cost {unchanged} bytes against the changed one's {changed}"
        );
    }
}
