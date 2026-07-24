//! Per-tile codec choice and encoding.
//!
//! Screen content is two very different things sharing one framebuffer. A menu,
//! a code editor, a terminal: a handful of flat colours and hard edges, where
//! PNG is both *smaller* and sharper than JPEG. A photo, a video, a map: a
//! continuous gradient where PNG barely compresses and JPEG wins by an order of
//! magnitude. Picking per tile gets both.
//!
//! The classifier has to be cheap enough to run on every tile of every frame, so
//! it samples rather than scanning: a strided subset of pixels into a small
//! bitset of distinct colours. Few distinct colours means UI or text, so PNG;
//! many means photographic, so JPEG.
//!
//! The gateway relays the result without looking inside, so the format byte
//! chosen here is what the browser's `createImageBitmap` is told.

use rxa_proto::msg::format;

/// JPEG quality. 80 keeps photographic tiles visually clean while staying well
/// under PNG's size for the same content; text never reaches this path.
const JPEG_QUALITY: u8 = 80;

/// Sample at most this many pixels when classifying a tile. A 64-row full-width
/// Retina strip is ~220k pixels; looking at 1024 of them decides the question
/// just as well and costs nothing.
const CLASSIFY_SAMPLES: usize = 1024;

/// Distinct sampled colours at or above which a tile is treated as
/// photographic. Flat UI stays in the tens; a photo saturates the sample almost
/// immediately. The gap between the two is wide, so the exact threshold is not
/// delicate.
const PHOTO_COLOUR_THRESHOLD: usize = 96;

/// Tiles below this many pixels always go to PNG: JPEG's own header costs more
/// than the payload at that size, and a tiny tile is usually a cursor-sized
/// piece of UI anyway.
const MIN_JPEG_PIXELS: usize = 32 * 32;

/// An encoded tile payload and the format byte that describes it.
pub struct Encoded {
    pub format: u8,
    pub data: Vec<u8>,
}

/// Encode packed RGB888 as PNG or JPEG, whichever suits the content.
pub fn encode_tile(w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Encoded> {
    let expected = usize::from(w) * usize::from(h) * 3;
    anyhow::ensure!(
        rgb.len() == expected,
        "tile payload is {} bytes, expected {expected} for {w}x{h} RGB",
        rgb.len()
    );
    if is_photographic(w, h, rgb) {
        Ok(Encoded {
            format: format::JPEG,
            data: encode_jpeg(w, h, rgb)?,
        })
    } else {
        Ok(Encoded {
            format: format::PNG,
            data: encode_png(w, h, rgb)?,
        })
    }
}

/// Cheap content classifier: count distinct colours over a strided sample.
///
/// Colours are quantised to 5 bits per channel before counting. Without that, a
/// smooth UI gradient — a window title bar, a selection highlight — reads as
/// hundreds of "distinct" colours and gets sent to JPEG, where its hard text
/// edges turn to mush. Quantising collapses the gradient while leaving genuinely
/// photographic content well over the threshold.
fn is_photographic(w: u16, h: u16, rgb: &[u8]) -> bool {
    let pixels = usize::from(w) * usize::from(h);
    if pixels < MIN_JPEG_PIXELS {
        return false;
    }
    // 15-bit quantised colour: 32768 possible values, one bit each.
    let mut seen = vec![0u64; 32768 / 64];
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

/// Same encoder and compression level the gateway uses for RDP/VNC tiles, so
/// a PNG tile from the agent is indistinguishable from one it produced itself.
fn encode_png(w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, u32::from(w), u32::from(h));
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgb)?;
    writer.finish()?;
    Ok(out)
}

fn encode_jpeg(w: u16, h: u16, rgb: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    encoder
        .encode(rgb, w, h, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| anyhow::anyhow!("JPEG encode failed: {e}"))?;
    Ok(out)
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

    /// Photographic: a wide, noisy, continuous-tone field.
    fn photo(w: u16, h: u16) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            for x in 0..w {
                let r = (u32::from(x) * 7 + u32::from(y) * 3) % 256;
                let g = (u32::from(x) * 3 + u32::from(y) * 11 + 40) % 256;
                let b = (u32::from(x) * 13 + u32::from(y) * 5 + 90) % 256;
                rgb.extend_from_slice(&[r as u8, g as u8, b as u8]);
            }
        }
        rgb
    }

    /// A smooth two-colour gradient — the case that must *not* go to JPEG,
    /// because a title bar's text sits on top of exactly this.
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

    #[test]
    fn flat_ui_goes_to_png() {
        let (w, h) = (320, 64);
        let tile = encode_tile(w, h, &flat_ui(w, h)).unwrap();
        assert_eq!(tile.format, format::PNG);
        assert_eq!(&tile.data[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn photographic_content_goes_to_jpeg() {
        let (w, h) = (320, 64);
        let tile = encode_tile(w, h, &photo(w, h)).unwrap();
        assert_eq!(tile.format, format::JPEG);
        // SOI marker.
        assert_eq!(&tile.data[..2], &[0xFF, 0xD8]);
    }

    // The reason the classifier quantises: a smooth UI gradient has many raw
    // colours but is not photographic, and JPEG would ruin the text on it.
    #[test]
    fn a_smooth_ui_gradient_is_not_mistaken_for_a_photo() {
        let (w, h) = (320, 64);
        assert!(!is_photographic(w, h, &ui_gradient(w, h)));
    }

    #[test]
    fn tiny_tiles_always_use_png() {
        // JPEG's fixed overhead would dominate, and small tiles are UI.
        let rgb = photo(16, 16);
        let tile = encode_tile(16, 16, &rgb).unwrap();
        assert_eq!(tile.format, format::PNG);
    }

    // Each codec has to actually beat the other on its own content, or the
    // classifier is just burning CPU to no purpose.
    #[test]
    fn each_codec_wins_on_the_content_it_is_chosen_for() {
        let (w, h) = (320, 64);
        let raw = usize::from(w) * usize::from(h) * 3;

        let ui = flat_ui(w, h);
        let ui_png = encode_png(w, h, &ui).unwrap().len();
        let ui_jpeg = encode_jpeg(w, h, &ui).unwrap().len();
        assert!(
            ui_png < ui_jpeg,
            "PNG should beat JPEG on flat UI: {ui_png} vs {ui_jpeg}"
        );

        let pic = photo(w, h);
        let pic_png = encode_png(w, h, &pic).unwrap().len();
        let pic_jpeg = encode_jpeg(w, h, &pic).unwrap().len();
        assert!(
            pic_jpeg < pic_png,
            "JPEG should beat PNG on photographic content: {pic_jpeg} vs {pic_png}"
        );
        assert!(pic_jpeg * 4 < raw, "JPEG should compress a photo well");
    }

    #[test]
    fn png_output_roundtrips_to_the_original_pixels() {
        let (w, h) = (64, 32);
        let rgb = flat_ui(w, h);
        let data = encode_png(w, h, &rgb).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(data.as_slice()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (u32::from(w), u32::from(h)));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(&buf[..info.buffer_size()], rgb.as_slice());
    }

    #[test]
    fn a_payload_of_the_wrong_length_is_rejected() {
        assert!(encode_tile(2, 2, &[0u8; 11]).is_err());
        assert!(encode_tile(2, 2, &[0u8; 13]).is_err());
        assert!(encode_tile(2, 2, &[0u8; 12]).is_ok());
    }

    // Every tile shape the capture path can produce must encode. A 1-pixel-tall
    // strip is the tail of a rect whose height is not a multiple of STRIP_ROWS.
    #[test]
    fn every_strip_shape_the_capture_path_produces_encodes() {
        for (w, h) in [(1, 1), (1, 64), (3456, 1), (3456, 64), (320, 22)] {
            let rgb = vec![128u8; usize::from(w) * usize::from(h) * 3];
            let tile = encode_tile(w, h, &rgb).unwrap();
            assert!(!tile.data.is_empty(), "{w}x{h} produced no payload");
        }
    }
}
