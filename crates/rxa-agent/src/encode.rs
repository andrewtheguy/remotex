//! Per-tile codec choice and encoding.
//!
//! Screen content is two very different things sharing one framebuffer. A menu,
//! a code editor, a terminal: a handful of flat colours and hard edges, where
//! lossless compression is both *smaller* and sharper than lossy. A photo, a
//! video, a map: a continuous gradient where lossless barely compresses and lossy
//! wins by an order of magnitude. Picking per tile gets both.
//!
//! The classifier has to be cheap enough to run on every tile of every frame, so
//! it samples rather than scanning: a strided subset of pixels into a small
//! bitset of distinct colours. Few distinct colours means UI or text, so
//! lossless; many means photographic, so lossy.
//!
//! Both are WebP, which is why that choice never leaves this file. It used to pick
//! between two *containers* — PNG and JPEG — and the format byte carried the
//! answer all the way to the browser's `createImageBitmap`. Now the container is
//! the same either way, the format byte has one value, and a misclassification
//! costs quality on one tile rather than sending a codec the other end has to
//! branch on.

use rxa_proto::msg::format;

/// Lossy quality. 80 keeps photographic tiles visually clean while staying well
/// under the lossless size for the same content; text never reaches this path.
const LOSSY_QUALITY: f32 = 80.0;

/// libwebp's speed/size dial, and — in lossless mode — the effort dial `quality`
/// becomes. Both at the cheap end, for the same reason the gateway's are
/// (`WEBP_LOSSLESS_METHOD` in `src/protocol.rs`, which records the measurements):
/// the compression above `method = 0` costs 20-80x the encode time for another
/// 10 points of ratio.
///
/// Neither is on an engine's protocol-read loop, and cells now encode several at a
/// time (`ENCODE_WIDTH` in `session.rs`) — but the budget is no larger for either of
/// those. At `method = 2` one 320x64 cell measured 4.1ms, so a dozen-cell Retina frame
/// would spend 40ms in the encoder, and a full repaint's 320 cells over a second. The
/// parallelism divides that by the cores it can get, which is the same answer the
/// gateway's `WEBP_LOSSLESS_METHOD` note reaches from the other direction: overlapping
/// a cost is not removing one, and here the cores are shared with the very desktop
/// being captured.
const WEBP_METHOD: i32 = 0;
const LOSSLESS_EFFORT: f32 = 20.0;

/// Sample at most this many pixels when classifying a tile. A 64-row full-width
/// Retina strip is ~220k pixels; looking at 1024 of them decides the question
/// just as well and costs nothing.
const CLASSIFY_SAMPLES: usize = 1024;

/// Distinct sampled colours at or above which a tile is treated as
/// photographic. Flat UI stays in the tens; a photo saturates the sample almost
/// immediately. The gap between the two is wide, so the exact threshold is not
/// delicate.
const PHOTO_COLOUR_THRESHOLD: usize = 96;

/// Tiles below this many pixels are always lossless: the lossy path's own header
/// costs more than the payload at that size, and a tiny tile is usually a
/// cursor-sized piece of UI anyway.
///
/// Cited by `CELL_W`'s documentation in `src/protocol.rs`, which needs a cell to
/// stay comfortably above it so the classifier has something to judge.
const MIN_LOSSY_PIXELS: usize = 32 * 32;

/// An encoded tile payload, the format byte that describes it, and whether the
/// pixels survived.
pub struct Encoded {
    pub format: u8,
    pub data: Vec<u8>,
    /// Which side of the classifier this came out of.
    ///
    /// Nothing on the wire needs it — both cases are WebP — but without it the
    /// classifier's decision would be unobservable from outside this module, and
    /// the tests that pin it would have nothing to assert on.
    pub lossless: bool,
}

/// Encode packed RGB888 as WebP, lossless or lossy to suit the content.
pub fn encode_tile(w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Encoded> {
    let expected = usize::from(w) * usize::from(h) * 3;
    anyhow::ensure!(
        rgb.len() == expected,
        "tile payload is {} bytes, expected {expected} for {w}x{h} RGB",
        rgb.len()
    );
    let lossless = !is_photographic(w, h, rgb);
    Ok(Encoded {
        format: format::WEBP,
        data: encode_webp(w, h, rgb, lossless)?,
        lossless,
    })
}

/// Cheap content classifier: count distinct colours over a strided sample.
///
/// Colours are quantised to 5 bits per channel before counting. Without that, a
/// smooth UI gradient — a window title bar, a selection highlight — reads as
/// hundreds of "distinct" colours and gets sent to the lossy path, where its hard
/// text edges turn to mush. Quantising collapses the gradient while leaving
/// genuinely photographic content well over the threshold.
fn is_photographic(w: u16, h: u16, rgb: &[u8]) -> bool {
    let pixels = usize::from(w) * usize::from(h);
    if pixels < MIN_LOSSY_PIXELS {
        return false;
    }
    // 15-bit quantised colour: 32768 possible values, one bit each. On the
    // stack — this runs on every tile of every frame, and 4KiB of zeroing beats
    // an allocation per tile.
    let mut seen = [0u64; 32768 / 64];
    let mut distinct = 0usize;
    let step = (pixels / CLASSIFY_SAMPLES).max(1);
    for i in (0..pixels).step_by(step) {
        let px = &rgb[i * 3..i * 3 + 3];
        let key = (usize::from(px[0] >> 3) << 10)
            | (usize::from(px[1] >> 3) << 5)
            | usize::from(px[2] >> 3);
        let (word, bit) = (key / 64, key % 64);
        if seen[word] & (1 << bit) == 0 {
            seen[word] |= 1 << bit;
            distinct += 1;
            if distinct >= PHOTO_COLOUR_THRESHOLD {
                return true;
            }
        }
    }
    false
}

/// Same encoder and settings the gateway uses for RDP/VNC tiles, so a lossless
/// tile from the agent is indistinguishable from one it produced itself.
///
/// Two hazards in the `webp` crate that the shape of this function answers:
/// `Encoder::from_rgb` *panics* on a buffer shorter than `w * h * 3`, so
/// `encode_tile`'s length check above is load-bearing rather than tidy; and
/// `Encoder::encode` and `encode_lossless` both `unwrap()` internally, so
/// `encode_advanced` is the only entry point that can report a failure.
///
/// No dimension check, unlike the gateway's: every caller here is a cell from
/// `split_cells`, so a side is at most `CELL_W`/`CELL_H` and cannot approach
/// WebP's 16383-pixel limit. Widening those constants past it would need one.
fn encode_webp(w: u16, h: u16, rgb: &[u8], lossless: bool) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(w > 0 && h > 0, "cannot encode a {w}x{h} tile");
    let mut config = webp::WebPConfig::new()
        .map_err(|()| anyhow::anyhow!("libwebp rejected its own default config"))?;
    config.lossless = i32::from(lossless);
    config.quality = if lossless { LOSSLESS_EFFORT } else { LOSSY_QUALITY };
    config.method = WEBP_METHOD;
    // No libwebp worker thread: the parallelism is outside this call, with
    // `ENCODE_WIDTH` cells of a frame (`session.rs`) encoding concurrently. Threads
    // inside each of those would fight the same cores for a much smaller split.
    config.thread_level = 0;
    let encoded = webp::Encoder::from_rgb(rgb, u32::from(w), u32::from(h))
        .encode_advanced(&config)
        .map_err(|e| anyhow::anyhow!("WebP encode failed for {w}x{h}: {e:?}"))?;
    // Copied out rather than held: `WebPMemory` is neither `Send` nor `Sync`, and
    // this payload is sent to another thread.
    Ok(encoded.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flat UI: a couple of colours, hard edges — a window with a border.
    fn flat_ui(w: u16, h: u16) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            for x in 0..w {
                let edge = x < 2 || y < 2 || x + 2 >= w || y + 2 >= h;
                if edge {
                    rgb.extend_from_slice(&[60, 60, 66]);
                } else if (y / 16) % 2 == 0 && (x / 7) % 3 == 0 {
                    rgb.extend_from_slice(&[20, 20, 24]); // "text"
                } else {
                    rgb.extend_from_slice(&[246, 246, 248]);
                }
            }
        }
        rgb
    }

    /// Photographic: a continuous-tone field with real *noise* in it.
    ///
    /// It used to be three linear ramps of `x` and `y`, which had the distinct-colour
    /// count of a photograph and none of its incompressibility. That was invisible
    /// while the lossless codec was PNG at `Compression::Fast`, which cannot exploit
    /// a ramp — but WebP's predictor transform can, and does: on the old fixture
    /// lossless came out *five times smaller* than lossy, so the test asserting
    /// otherwise failed the moment the codec changed. A fixture that is only
    /// photographic by one metric proves nothing about a codec that reads another.
    fn photo(w: u16, h: u16) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            for x in 0..w {
                // A smooth base so it still looks like a photograph to the
                // classifier, plus enough noise that prediction cannot win.
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // Enough to defeat exact prediction, small enough that the field
                // still has the local correlation a photograph has — pure white
                // noise is section 4 of the gateway's bench, not a photograph.
                let jitter = ((state >> 56) as i32 - 128) / 5;
                let base = [
                    u32::from(x) * 7 + u32::from(y) * 3,
                    u32::from(x) * 3 + u32::from(y) * 11 + 40,
                    u32::from(x) * 13 + u32::from(y) * 5 + 90,
                ];
                for channel in base {
                    rgb.push(((channel as i32 % 256 + jitter).clamp(0, 255)) as u8);
                }
            }
        }
        rgb
    }

    /// A smooth two-colour gradient — the case that must *not* go to the lossy
    /// path, because a title bar's text sits on top of exactly this.
    fn ui_gradient(w: u16, h: u16) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            let v = 200 + (u32::from(y) * 40 / u32::from(h.max(1))) as u8;
            for _ in 0..w {
                rgb.extend_from_slice(&[v, v, v + 2]);
            }
        }
        rgb
    }

    /// Every payload is a WebP now, so the container no longer says which branch
    /// ran — the `lossless` flag does, and this checks both agree with the bytes.
    fn assert_is_webp(data: &[u8]) {
        assert_eq!(&data[..4], b"RIFF");
        assert_eq!(&data[8..12], b"WEBP");
    }

    #[test]
    fn flat_ui_is_encoded_losslessly() {
        let (w, h) = (320, 64);
        let tile = encode_tile(w, h, &flat_ui(w, h)).unwrap();
        assert_eq!(tile.format, format::WEBP);
        assert!(tile.lossless);
        assert_is_webp(&tile.data);
    }

    #[test]
    fn photographic_content_is_encoded_lossily() {
        let (w, h) = (320, 64);
        let tile = encode_tile(w, h, &photo(w, h)).unwrap();
        assert_eq!(tile.format, format::WEBP);
        assert!(!tile.lossless);
        // Same container as the lossless branch, which is the point: the client
        // needs no signal to tell them apart.
        assert_is_webp(&tile.data);
    }

    // The reason the classifier quantises: a smooth UI gradient has many raw
    // colours but is not photographic, and lossy would ruin the text on it.
    #[test]
    fn a_smooth_ui_gradient_is_not_mistaken_for_a_photo() {
        let (w, h) = (320, 64);
        assert!(!is_photographic(w, h, &ui_gradient(w, h)));
    }

    #[test]
    fn tiny_tiles_are_always_lossless() {
        // The lossy path's fixed overhead would dominate, and small tiles are UI.
        let rgb = photo(16, 16);
        let tile = encode_tile(16, 16, &rgb).unwrap();
        assert!(tile.lossless);
    }

    // Each branch has to actually beat the other on its own content, or the
    // classifier is just burning CPU to no purpose.
    #[test]
    fn each_branch_wins_on_the_content_it_is_chosen_for() {
        let (w, h) = (320, 64);
        let raw = usize::from(w) * usize::from(h) * 3;

        let ui = flat_ui(w, h);
        let ui_lossless = encode_webp(w, h, &ui, true).unwrap().len();
        let ui_lossy = encode_webp(w, h, &ui, false).unwrap().len();
        assert!(
            ui_lossless < ui_lossy,
            "lossless should beat lossy on flat UI: {ui_lossless} vs {ui_lossy}"
        );

        let pic = photo(w, h);
        let pic_lossless = encode_webp(w, h, &pic, true).unwrap().len();
        let pic_lossy = encode_webp(w, h, &pic, false).unwrap().len();
        assert!(
            pic_lossy < pic_lossless,
            "lossy should beat lossless on photographic content: {pic_lossy} vs {pic_lossless}"
        );
        assert!(pic_lossy * 4 < raw, "lossy should compress a photo well");
    }

    #[test]
    fn the_lossless_branch_roundtrips_to_the_original_pixels() {
        let (w, h) = (64, 32);
        let rgb = flat_ui(w, h);
        let data = encode_webp(w, h, &rgb, true).unwrap();
        let image = webp::Decoder::new(&data).decode().expect("payload decodes");
        assert_eq!((image.width(), image.height()), (u32::from(w), u32::from(h)));
        // An RGB source must not gain an alpha channel: both clients discard alpha
        // on the stated grounds that tiles are opaque, so one that carried it would
        // decode to the wrong pixels rather than fail.
        assert!(!image.is_alpha());
        assert_eq!(&*image, rgb.as_slice());
    }

    // And the other branch must *not* roundtrip, or `LOSSY_QUALITY` is being
    // ignored and every photographic tile is paying lossless prices.
    #[test]
    fn the_lossy_branch_really_is_lossy() {
        let (w, h) = (64, 64);
        let rgb = photo(w, h);
        let data = encode_webp(w, h, &rgb, false).unwrap();
        let image = webp::Decoder::new(&data).decode().expect("payload decodes");
        assert_eq!((image.width(), image.height()), (u32::from(w), u32::from(h)));
        assert_ne!(&*image, rgb.as_slice());
    }

    #[test]
    fn a_payload_of_the_wrong_length_is_rejected() {
        assert!(encode_tile(2, 2, &[0u8; 11]).is_err());
        assert!(encode_tile(2, 2, &[0u8; 13]).is_err());
        assert!(encode_tile(2, 2, &[0u8; 12]).is_ok());
    }

    // Every tile shape the capture path can produce must encode. A 1-pixel-tall
    // strip is the tail of a rect whose height is not a multiple of CELL_H.
    #[test]
    fn every_cell_shape_the_capture_path_produces_encodes() {
        for (w, h) in [(1, 1), (1, 64), (320, 1), (320, 64), (320, 22)] {
            let rgb = vec![128u8; usize::from(w) * usize::from(h) * 3];
            let tile = encode_tile(w, h, &rgb).unwrap();
            assert!(!tile.data.is_empty(), "{w}x{h} produced no payload");
            assert_is_webp(&tile.data);
        }
    }

    #[test]
    fn a_zero_sized_tile_is_rejected_rather_than_handed_to_libwebp() {
        // libwebp fails these deep inside with BAD_DIMENSION, and
        // `Encoder::from_rgb` panics outright on a buffer shorter than w*h*3.
        assert!(encode_tile(0, 4, &[]).is_err());
        assert!(encode_tile(4, 0, &[]).is_err());
    }
}
