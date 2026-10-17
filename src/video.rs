//! What every picture shares with the rest of the gateway: the framebuffer copy the
//! tiles are cut from ([`Mirror`]) and the picture ceiling every video stream passed
//! is held under ([`check_picture`]). When a round is taken is
//! [`crate::encode`]'s business, and what it codes is [`crate::tiles`]'.
//!
//! **Accepted pixels are owed until a tile carries them.** [`crate::shadow::Shadow`]
//! records source pixels as delivered the moment it accepts them, and nothing
//! re-sends them, so the tiles they land on stay owed until a round takes them. The
//! mirror holds the whole framebuffer because damage arrives as rectangles, and VNC's
//! can only be cropped out of the one it just decoded.
//!
//! A detach drops what was queued (`SessionManager::pump`), so a client coming back has
//! missed part of the picture. It recovers because every attach injects a repaint,
//! which owes it every tile from the picture's start.

use crate::shadow::Rect;

/// The 1–100 quality dial, coarsest first.
///
/// The dial is the gateway's own scale: a tile's JPEG quality, which is the same 1–100,
/// and the ceiling a `wlshare` target asks wlshare's VP9 for, which wlshare maps onto
/// its own quantizer.
pub const QUALITY_MIN: u8 = 1;
/// See [`QUALITY_MIN`].
pub const QUALITY_MAX: u8 = 100;

/// The longest side a passed video stream's picture may have, and the longest the
/// *other* side may then be.
///
/// Checked either way up, because the limit is on the picture and not on width:
/// 3840×2400 and 2400×3840 are both legal, and 3840×3840 is not. A `w <= 3840 &&
/// h <= 2400` test would read almost the same and would refuse a portrait desktop.
///
/// The ceiling is a *stream's*: a picture of the whole desktop that the remote codes
/// and the gateway passes, wlshare's VP9, a High Performance Mac's HEVC or H.264,
/// and an RDP host's graphics pipeline. 4K is the *panel*, in either shape: 3840×2160
/// is a 16:9 one and 3840×2400 the 16:10 one a 1920×1200 laptop at 2x is. 5K is
/// past the line, and past the level a browser's decoder is asked for.
///
/// A picture that is not such a stream has no ceiling: the tiles coded here, of a
/// plain VNC server's desktop or an RDP host's bitmap path, and a Standard Mac's own
/// rectangles, passed one by one, are shown at whatever size the remote has, a
/// Combined Display of several screens included. A session on one of them is sized
/// by the remote, and what it sends is shown.
pub const MAX_LONG_SIDE: u16 = 3840;
/// See [`MAX_LONG_SIDE`].
pub const MAX_SHORT_SIDE: u16 = 2400;

/// Whether a picture of this many pixels is under the ceiling — the test
/// [`check_picture`] refuses on, for a caller deciding what to ask a remote for.
pub fn within_ceiling((w, h): (u32, u32)) -> bool {
    let (long, short) = (w.max(h), w.min(h));
    long <= u32::from(MAX_LONG_SIDE) && short <= u32::from(MAX_SHORT_SIDE)
}

/// `pixels` held under the ceiling, the way High Performance holds a virtual
/// display under the Mac's: each axis clamped, the orientation kept, so a
/// landscape desktop is held to 3840×2400 and a portrait one to 2400×3840.
///
/// This is what lets an engine whose picture is a passed stream ask the remote for
/// a desktop under the ceiling instead of ending the session over the one it was
/// about to get: a 5120×2880 screen, or a 2560×1440 one at 2x, opens and resizes
/// to a 3840×2400 desktop shown at 100% with the rest of the window left bare. Not
/// a scale — the gateway never shrinks a picture, every pixel the remote draws
/// reaches the browser as drawn — so the remote is the one asked for less. Per
/// axis and not by aspect, as the Mac's ceiling is: it keeps the most of the window.
pub fn fit_ceiling((w, h): (u32, u32)) -> (u32, u32) {
    let (long, short) = (u32::from(MAX_LONG_SIDE), u32::from(MAX_SHORT_SIDE));
    if w >= h {
        (w.min(long), h.min(short))
    } else {
        (w.min(short), h.min(long))
    }
}

/// The whole framebuffer as packed RGB888, which the tiles are cut from: the desktop
/// at its own size, odd sides included.
#[derive(Clone)]
pub struct Mirror {
    /// The desktop as the client knows it, and as a record header reports it.
    size: (u16, u16),
    rgb: Vec<u8>,
}

impl Mirror {
    /// A mirror for a `w`×`h` desktop. ~6 MB at 1080p.
    pub fn new(w: u16, h: u16) -> anyhow::Result<Self> {
        anyhow::ensure!(w > 0 && h > 0, "a video mirror cannot hold a {w}x{h} desktop");
        Ok(Self { size: (w, h), rgb: vec![0; usize::from(w) * usize::from(h) * 3] })
    }

    /// The desktop this mirror is for.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Its pixels, packed RGB888 row by row: what a composition is averaged from
    /// ([`crate::combined::Compositor`]).
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    /// Copy `rgb` — packed RGB888 for `rect` — into the mirror.
    ///
    /// A rectangle outside the desktop is refused rather than clipped: it means this
    /// mirror and the engine disagree about how big the framebuffer is, and taking part
    /// of the update would put that disagreement on the screen instead of in the log.
    pub fn blit(&mut self, rect: Rect, rgb: &[u8]) -> anyhow::Result<()> {
        let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
        anyhow::ensure!(
            rgb.len() == w * h * 3,
            "a video blit is {} bytes, expected {} for {w}x{h} RGB",
            rgb.len(),
            w * h * 3
        );
        anyhow::ensure!(
            rect.right < self.size.0 && rect.bottom < self.size.1,
            "a video blit of {w}x{h} at ({},{}) falls outside a {}x{} desktop",
            rect.left,
            rect.top,
            self.size.0,
            self.size.1
        );

        let stride = usize::from(self.size.0) * 3;
        for row in 0..h {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            self.rgb[at..at + w * 3].copy_from_slice(&rgb[row * w * 3..(row + 1) * w * 3]);
        }
        Ok(())
    }

    /// `rect`'s pixels as packed RGB888, into a buffer the caller reuses: a run of
    /// tiles a round takes ([`crate::tiles::Tiles::take_round`]).
    pub fn crop_into(&self, rect: Rect, out: &mut Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(
            rect.right < self.size.0 && rect.bottom < self.size.1,
            "a video crop of {}x{} at ({},{}) falls outside a {}x{} mirror",
            rect.w(),
            rect.h(),
            rect.left,
            rect.top,
            self.size.0,
            self.size.1
        );
        let stride = usize::from(self.size.0) * 3;
        let (w, h) = (usize::from(rect.w()), usize::from(rect.h()));
        out.clear();
        out.reserve(w * h * 3);
        for row in 0..h {
            let at = (usize::from(rect.top) + row) * stride + usize::from(rect.left) * 3;
            out.extend_from_slice(&self.rgb[at..at + w * 3]);
        }
        Ok(())
    }
}

/// Refuse a stream's picture past the ceiling.
///
/// That cannot be a config-time refusal — only the remote knows its own size, and it
/// may change mid-session — so the message has to carry the whole explanation to
/// wherever it surfaces.
pub fn check_picture((w, h): (u16, u16)) -> anyhow::Result<()> {
    anyhow::ensure!(
        within_ceiling((u32::from(w), u32::from(h))),
        "the gateway will not show a {w}x{h} picture as a video stream: one is refused \
         with a long side over {MAX_LONG_SIDE} or a short side over {MAX_SHORT_SIDE}. \
         Only the remote knows its own size, so check-config cannot catch this — give \
         this target a remote that can be asked for a smaller desktop: in a session \
         started with resize the gateway holds every size it asks for under this ceiling"
    );
    Ok(())
}

#[cfg(test)]
impl Mirror {
    /// The pixel at `(x, y)`, for the tests about blitting.
    pub(crate) fn pixel(&self, x: u16, y: u16) -> [u8; 3] {
        let at = (usize::from(y) * usize::from(self.size.0) + usize::from(x)) * 3;
        [self.rgb[at], self.rgb[at + 1], self.rgb[at + 2]]
    }

    pub(crate) fn len(&self) -> usize {
        self.rgb.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle from a position and a size, which is what most of these want.
    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::from_size(x, y, w, h).expect("a rectangle with a size")
    }

    /// `w`×`h` of one colour, which makes "did these pixels land here" readable a byte
    /// at a time.
    fn flat(w: u16, h: u16, colour: [u8; 3]) -> Vec<u8> {
        colour
            .iter()
            .copied()
            .cycle()
            .take(usize::from(w) * usize::from(h) * 3)
            .collect()
    }

    #[test]
    fn a_blit_lands_where_the_rectangle_says() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        mirror
            .blit(rect(33, 41, 17, 9), &flat(17, 9, [200, 100, 50]))
            .expect("a blit inside the desktop");
        assert_eq!(mirror.pixel(33, 41), [200, 100, 50]);
        assert_eq!(mirror.pixel(49, 49), [200, 100, 50]);
        // One pixel outside each edge is still the black the mirror started as.
        assert_eq!(mirror.pixel(32, 41), [0, 0, 0]);
        assert_eq!(mirror.pixel(50, 41), [0, 0, 0]);
        assert_eq!(mirror.pixel(33, 40), [0, 0, 0]);
        assert_eq!(mirror.pixel(33, 50), [0, 0, 0]);
    }

    #[test]
    fn a_blit_outside_the_desktop_is_refused_rather_than_clipped() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        let too_far = mirror.blit(rect(630, 470, 20, 20), &flat(20, 20, [1, 2, 3]));
        assert!(too_far.is_err(), "a rectangle off the edge of the desktop was accepted");
        let wrong_size = mirror.blit(rect(0, 0, 16, 16), &flat(16, 15, [1, 2, 3]));
        assert!(wrong_size.is_err(), "pixels that are not the rectangle's were accepted");
    }

    #[test]
    fn a_crop_reads_the_rectangle_it_names() {
        let mut mirror = Mirror::new(640, 480).expect("a mirror");
        mirror
            .blit(rect(320, 64, 320, 64), &flat(320, 64, [9, 8, 7]))
            .expect("a blit");
        let mut out = Vec::new();
        mirror.crop_into(rect(320, 64, 320, 64), &mut out).expect("a crop of that rectangle");
        assert_eq!(out, flat(320, 64, [9, 8, 7]));
        // Its neighbour is untouched, which is the same statement from the other side.
        mirror.crop_into(rect(0, 64, 320, 64), &mut out).expect("a crop of the rectangle beside it");
        assert_eq!(out, flat(320, 64, [0, 0, 0]));
        // A rectangle the mirror does not hold is refused rather than indexed.
        assert!(mirror.crop_into(rect(600, 400, 64, 128), &mut out).is_err());
    }

    #[test]
    fn an_odd_desktop_is_held_at_its_own_size() {
        let mut mirror = Mirror::new(1919, 1079).expect("a mirror");
        assert_eq!(mirror.size(), (1919, 1079));
        assert_eq!(mirror.len(), 1919 * 1079 * 3);
        mirror.blit(rect(1918, 1078, 1, 1), &[1, 2, 3]).expect("a blit of the last pixel");
        assert_eq!(mirror.pixel(1918, 1078), [1, 2, 3]);
    }

    #[test]
    fn a_picture_too_large_is_refused_by_name() {
        // Straight to `check_picture` rather than through a 44 MB allocation: a mirror
        // will hold whatever it is asked to, and the refusal being tested is the
        // ceiling's.
        let Err(refused) = check_picture((5120, 2880)) else {
            panic!("a 5K picture was accepted");
        };
        let message = format!("{refused:#}");
        assert!(message.contains("5120x2880"), "the message does not say what was asked for");
        assert!(message.contains("3840"), "the message does not say what the limit is");
        assert!(message.contains("resize"), "the message does not say what to do instead");
        // Both 4K panels are pictures: the 16:9 one and the 16:10 one a 1920×1200 laptop
        // is at 2x.
        assert!(check_picture((3840, 2160)).is_ok(), "16:9 4K was refused");
        assert!(check_picture((3840, 2400)).is_ok(), "16:10 4K was refused");
        // The limit is on the picture, not on width: turning a legal desktop on its side
        // does not make it illegal.
        assert!(
            check_picture((2400, 3840)).is_ok(),
            "a portrait 4K desktop was refused"
        );
        assert!(Mirror::new(0, 1080).is_err(), "a desktop with no pixels was accepted");
    }

    /// The size a resizing engine asks for instead: each axis held, the
    /// orientation kept, and anything already inside the ceiling untouched.
    #[test]
    fn a_desktop_is_held_under_the_ceiling_per_axis() {
        assert_eq!(fit_ceiling((5120, 2880)), (3840, 2400), "a 5K screen");
        assert_eq!(fit_ceiling((2880, 5120)), (2400, 3840), "the same screen on its side");
        assert_eq!(fit_ceiling((1920, 1200)), (1920, 1200), "a desktop under the ceiling");
        assert_eq!(fit_ceiling((3840, 2160)), (3840, 2160), "16:9 4K is under the ceiling");
        assert_eq!(fit_ceiling((3840, 2400)), (3840, 2400), "the ceiling itself");
        for size in [(5120, 2880), (2880, 5120), (4000, 4000), (3840, 2401)] {
            assert!(!within_ceiling(size), "{size:?} is over the ceiling");
            let held = fit_ceiling(size);
            assert!(within_ceiling(held), "{size:?} held to {held:?} is still over it");
            assert!(
                check_picture((held.0 as u16, held.1 as u16)).is_ok(),
                "the ceiling refuses the {held:?} the engine was told to ask for"
            );
        }
    }
}
