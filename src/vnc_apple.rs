//! Apple's RFB 003.889 messages and encodings — what the `ard-high-performance`
//! subtype speaks on top of the record layer in [`crate::vnc_record`].
//!
//! Everything here is either a message this client builds or a rectangle payload
//! it parses. The transport is [`crate::vnc_record`]'s and the session loop is
//! [`crate::vnc`]'s; this module is pure and has no I/O, which is what lets the
//! wire formats be asserted byte for byte.
//!
//! ## What the extension is for, here
//!
//! **Compression**: standard zlib ([`ENCODING_ZLIB`]) instead of raw pixels, which
//! is around fifty times fewer bytes on a static desktop. Apple's own still-image
//! codecs would do better still, but their payload formats are unresolved in the
//! reference this was written from, and a client must not advertise an encoding it
//! cannot decode.
//!
//! **Picking a screen**: [`ENCODING_DISPLAY_LAYOUT`] carries the Mac's real
//! displays and [`set_display_message`] binds one of them, narrowing the
//! framebuffer to that screen's own pixels.
//!
//! **The pixel density**, which comes with it. Each screen states its own scale,
//! so a 2x display arrives at 3200x1800 and is reported as 1600x900 points — the
//! desktop then draws at 100% instead of twice its size. Because the density is
//! per screen, the *combined* view of a mixed-density Mac has no single scale (see
//! [`Layout::scale`]), which makes picking a screen the thing that makes the
//! geometry exact rather than a convenience.
//!
//! ## `SetDisplayConfiguration` is deliberately not sent
//!
//! Sending it is what made this wire look like it could do none of the above. A
//! `0x1d` descriptor — even the bare static one, which the reference presents as
//! asking for the Mac's real screens — makes macOS 26 **create a virtual display**
//! sized to the union of the real ones, deactivate them, and report a one-screen
//! layout with a fresh `CGDirectDisplayID` each session and a flat density of 1.
//! Omitting the message entirely is what gets the real screens. Measured both
//! ways, repeatedly; docs/apple-vnc-889.md has the transcripts.
//!
//! ## What is otherwise absent
//!
//! No dynamic resolution, so `resize` is refused for this subtype at config load.
//! No Adaptive media path (HEVC/AAC over SRTP), so `RFBMediaStreamMessage1` is
//! never advertised. No pasteboard: on this wire the Mac carries the clipboard over
//! messages of its own rather than RFB's, so `clipboard` is refused too.
//!
//! ## Reading the offsets in here
//!
//! The reference document is reverse-engineered and says so; several offsets in it
//! disagree with the bytes a live Mac sends — a display record's fields are
//! uniformly two bytes later than it claims — and where they do the comments name
//! which reading is implemented and what measurement settled it. Payload hex is
//! logged at `debug` on first receipt for exactly that reason.

use std::collections::HashMap;

use anyhow::Context as _;
use log::{debug, warn};

use crate::protocol::{CursorShape, DisplayInfo};

/// Raw pixels, the standard RFB encoding, still the fallback here.
const ENCODING_RAW: i32 = 0;
/// Standard RFB `DesktopSize` and `LastRect`, both of which have to be in
/// [`ENCODINGS`] and so are named here rather than reached for across modules.
const ENCODING_DESKTOP_SIZE: i32 = -223;
const ENCODING_LAST_RECT: i32 = -224;
/// Standard RFB zlib: `u32 length` then that many bytes of one deflate stream
/// shared by every rectangle on the connection.
pub const ENCODING_ZLIB: i32 = 0x06;
/// The record layer's key, delivered as a rectangle before the record layer
/// exists. See [`crate::vnc_record`].
pub const ENCODING_REKEY: i32 = 0x44f;
/// Cursor shapes, as a server-side cache of pixmaps that are stored once and then
/// selected by id. See [`CursorCache`].
pub const ENCODING_CURSOR_IMAGE: i32 = 0x450;
/// The Mac's displays and the geometry it is rendering at. See [`parse_layout`].
pub const ENCODING_DISPLAY_LAYOUT: i32 = 0x451;
/// Apple's four vendor keysyms. Parsed and dropped: nothing here sends them.
pub const ENCODING_VENDOR_KEYSYMS: i32 = 0x453;
/// The Mac's current keyboard input source. Parsed and dropped.
pub const ENCODING_KEYBOARD_SOURCE: i32 = 0x455;
/// The Mac's model and enclosure colour. Parsed and dropped. Not advertised — see
/// [`ENCODINGS`] — but tolerated, because it arrives anyway on some builds.
pub const ENCODING_DEVICE_INFO: i32 = 0x456;
/// The pointer's position, in the rectangle header and with no payload. Advertised
/// and ignored: a client draws the pointer where it last put it.
pub const ENCODING_CURSOR_POS: i32 = 0x44c;
/// An older, simpler display list than [`ENCODING_DISPLAY_LAYOUT`], carrying no
/// density. Advertised and stepped over; macOS 26 never sent one.
pub const ENCODING_DISPLAY_INFO: i32 = 0x44d;
/// The logged-in user's name and avatar. Not advertised — see [`ENCODINGS`] — but
/// decoded, because it arrives anyway on some builds and its framing is nothing like
/// the other metadata encodings'.
pub const ENCODING_USER_INFO: i32 = 0x44e;

/// What this client advertises in `SetEncodings` **first**, before the Mac has said
/// anything about its displays.
///
/// **Do not add to, remove from, or reorder this list.** It is not a preference
/// order, it is the list that makes macOS 26 report its displays at all, and it was
/// arrived at by measurement rather than by reasoning. Every single-entry addition
/// tried (zlib, `DeviceInfo`), every single-entry removal, and even reversing the
/// order while keeping the set produced a session with **no
/// [`ENCODING_DISPLAY_LAYOUT`] at all** — no display list, no per-screen density,
/// nothing to pick — while this sequence produced one every time. Sixteen variants,
/// one connection each, no exceptions. Why the daemon reads the list this way is
/// unresolved; that it does is not.
///
/// One caveat, recorded because the alternative is a comment that reads stricter than
/// the evidence: the bisected list also carried `ENCODING_USER_INFO`, which this one
/// does not, and that particular removal was never among the variants tried. It was
/// checked the other way instead — this exact list, against the same Mac, produced a
/// layout and a working selection. See docs/apple-vnc-889.md.
///
/// The order is noVNC-ARD's relative order, which is the only other client known to
/// receive a layout from a real Mac.
///
/// Every entry is decoded or deliberately stepped over, which is a requirement and
/// not a courtesy: a server takes the list as a promise and will send what it finds
/// here. That is why Apple's own still-image codecs are absent — the reference
/// leaves their payload formats unresolved, so advertising them would ask for
/// rectangles this client could only guess at.
pub const ENCODINGS: &[i32] = &[
    ENCODING_RAW,
    ENCODING_CURSOR_POS,
    ENCODING_DISPLAY_INFO,
    ENCODING_REKEY,
    ENCODING_CURSOR_IMAGE,
    ENCODING_DISPLAY_LAYOUT,
    ENCODING_VENDOR_KEYSYMS,
    ENCODING_KEYBOARD_SOURCE,
    ENCODING_DESKTOP_SIZE,
    ENCODING_LAST_RECT,
];

/// And what it advertises once a layout has arrived: the same list with zlib on the
/// end.
///
/// The list above cannot carry zlib — adding it anywhere costs the display layout —
/// so compression is asked for in a second `SetEncodings` after the Mac has already
/// reported its screens. It keeps that state and simply switches encoder: measured
/// at 398 KB for a 3200x1800 frame against 23 MB of raw pixels, with display
/// selection still working afterwards. Sending only the first list would leave the
/// subtype slower than plain `ard`; sending only a list with zlib in it would leave
/// it with nothing to pick.
pub const ENCODINGS_WITH_ZLIB: &[i32] = &[
    ENCODING_RAW,
    ENCODING_CURSOR_POS,
    ENCODING_DISPLAY_INFO,
    ENCODING_REKEY,
    ENCODING_CURSOR_IMAGE,
    ENCODING_DISPLAY_LAYOUT,
    ENCODING_VENDOR_KEYSYMS,
    ENCODING_KEYBOARD_SOURCE,
    ENCODING_DESKTOP_SIZE,
    ENCODING_LAST_RECT,
    ENCODING_ZLIB,
];

/// Bytes of one display record in a layout payload.
const LAYOUT_RECORD: usize = 0x38;
/// Bytes of a display record this parser actually reads. The rest is a pixel format
/// it has no use for, and is where the final record's missing two bytes come from —
/// see [`parse_layout`].
const LAYOUT_FIELDS: usize = 0x2a;
/// Bytes of a layout payload before its first record, *including* the `u16`
/// length prefix — see [`parse_layout`] for why that is the reading used.
const LAYOUT_HEAD: usize = 0x14;
/// Largest cursor edge accepted, matching the plain RFB path. Real pointers are
/// 32x32 or 64x64.
const MAX_CURSOR_DIM: u16 = 256;
/// Ceiling on one inflated payload, so a hostile or broken stream cannot be
/// answered with unbounded memory.
///
/// An 8192x8192 framebuffer at four bytes a pixel, which is past any real Mac and
/// comfortably past the 4480x1800 (31 MiB) a two-display session synthesizes. It
/// used to be 64 MiB, which is *tighter than the raw path*: a rectangle the raw
/// branch in [`crate::vnc`] happily allocates would be refused here for no reason
/// but the codec it arrived under. The real bound on either path is the rectangle's
/// bounds check against the announced desktop; this only stops a wildly bogus
/// geometry from turning into an allocation.
const MAX_INFLATED: usize = 8192 * 8192 * 4;

/// `SetEncryption(command = 1)`: turn the record layer on, AES-128 the only
/// method there is.
///
/// The bytes are the observed message verbatim. The reference's field list
/// accounts for eight of them and the wire carries twelve, so the trailing word
/// is copied rather than derived — a message the server pattern-matches is not
/// the place to send a tidier version of itself.
pub fn set_encryption_start() -> Vec<u8> {
    vec![0x12, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01]
}

/// `SetEncryption(command = 2)`.
///
/// Nominally "stop encryption", which it plainly is not: native Screen Sharing
/// sends this immediately after `command = 1` and the session that follows is
/// encrypted throughout. Sent because native sends it.
pub fn set_encryption_stop() -> Vec<u8> {
    vec![0x12, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00]
}

/// `AutoFrameBufferUpdate`: hand the update cycle to the server.
///
/// Standard RFB is a poll — one request, one update, repeat. This switches the
/// server to sending on its own, which is both faster and the *only* way Apple's
/// server-driven rectangles (cursor shapes above all) keep arriving. It has to be
/// re-sent whenever the Mac changes its display layout, because a login, a lock
/// or a fast-user-switch quietly drops the arming and the symptom is a pointer
/// frozen on its last shape rather than an error.
pub fn auto_framebuffer_update((w, h): (u16, u16)) -> Vec<u8> {
    let mut msg = Vec::with_capacity(16);
    msg.push(0x09);
    msg.push(0);
    msg.extend_from_slice(&1u16.to_be_bytes()); // version
    // Update interval, zero for the server default. This is not a display id:
    // `SetDisplayMessage` is the one and only place a screen is selected.
    msg.extend_from_slice(&0u32.to_be_bytes());
    for value in [0, 0, w, h] {
        msg.extend_from_slice(&value.to_be_bytes());
    }
    msg
}

/// `SetDisplayMessage`: share this one display, or all of them.
///
/// Measured to work both ways on macOS 26. `combine_all` puts every screen in one
/// framebuffer and the Mac then reports `current_display` as the `0xffffffff`
/// sentinel; naming an id narrows the framebuffer to that screen's own pixel size,
/// and the Mac echoes the id back in the next layout. That echo is the whole
/// confirmation protocol — see [`Layout::current`].
pub fn set_display_message(id: Option<u32>) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8);
    msg.push(0x0d);
    msg.push(u8::from(id.is_none())); // combine_all_displays
    msg.extend_from_slice(&0u16.to_be_bytes()); // reserved
    // Ignored by the server when combining, and zero is what native sends there.
    msg.extend_from_slice(&id.unwrap_or(0).to_be_bytes());
    msg
}

/// One screen out of a display layout: what a client is offered, plus the two
/// facts the gateway needs about it that a client never sees.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    pub info: DisplayInfo,
    /// Pixels per point on *this* screen, as the Mac states it: the `f64` at
    /// `+0x02`. 1.0 or 2.0 on every Mac measured, and cross-checked against the
    /// screen's own two bounds rects, which must agree.
    pub density: f32,
    /// This screen's backing-pixel size. The full repaint after a combined
    /// layout consists of one such region per non-mirrored display; gaps in the
    /// bounding framebuffer are not rectangles the Mac sends.
    pub backing: (u16, u16),
}

/// The Mac's display layout: which screens it has, which one it is sending, and
/// the framebuffer size that follows from that.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The framebuffer's size in pixels — what rectangles are addressed in. From
    /// the header, and the authoritative value: it tracks a selection, where the
    /// per-screen rects do not.
    pub backing: (u16, u16),
    /// The screen the Mac says it is sending, or `None` for the combined view of
    /// all of them (the `0xffffffff` sentinel).
    pub current: Option<u32>,
    pub displays: Vec<Display>,
}

impl Layout {
    /// Pixels per point, for [`crate::protocol::ServerMsg::Resize`].
    ///
    /// The density of the *selected* screen, because that is the only place a
    /// single number is true. The combined view is a mosaic of screens at
    /// different densities — the measured Mac puts a 1x 1280x800 beside a 2x
    /// 1600x900, giving a 4480x1800 framebuffer of 2880x900 points — and no one
    /// scale describes it. There it reports [`UNSCALED`], which shows the
    /// framebuffer at its pixel size: too large on the Retina half, but nothing
    /// is misrepresented, and picking a screen is what makes it exact.
    ///
    /// [`UNSCALED`]: crate::protocol::UNSCALED
    pub fn scale(&self) -> f32 {
        let Some(id) = self.current else {
            return crate::protocol::UNSCALED;
        };
        self.displays
            .iter()
            .find(|d| d.info.id == id)
            .map_or(crate::protocol::UNSCALED, |d| d.density)
    }

    /// The screens as a client is offered them.
    pub fn infos(&self) -> Vec<DisplayInfo> {
        self.displays.iter().map(|d| d.info.clone()).collect()
    }

    /// Pixels a non-incremental update must cover before polling may resume.
    ///
    /// A selected display fills its framebuffer. The combined framebuffer can
    /// contain gaps around unequal screens, so its full paint is the sum of the
    /// real display regions rather than the bounding width times height.
    pub fn repaint_pixels(&self) -> u64 {
        if self.current.is_some() {
            return u64::from(self.backing.0) * u64::from(self.backing.1);
        }
        self.displays
            .iter()
            .map(|display| u64::from(display.backing.0) * u64::from(display.backing.1))
            .sum()
    }
}

/// Parse an `AppleDisplayLayout` payload.
///
/// ## The offsets are two bytes later than the reference says
///
/// Every field of a display record sits at the reference's offset **plus two**.
/// That is not a guess: the tell is the `f64` `3ff0000000000000` (1.0), which the
/// reference puts at `+0x00` and `+0x08` and a live Mac puts at `+0x02` and
/// `+0x0a`. Reading the reference's offsets yields `display_id = 0` for every
/// screen and denormal garbage for the scales, which is what this parser used to
/// do. The shifted offsets reproduce the measured VM exactly — ids 1 and 4, a
/// 1280x800 at (0,0) and a 1600x900 at (1280,0), main on the first — so they are
/// the ones implemented. See docs/apple-vnc-889.md.
///
/// Both rects are `(top, left, bottom, right)`, not the `(x, y, w, h)` the
/// reference models; a size is a difference of edges here.
///
/// ## The length rule, which is off by two
///
/// The `u16` prefix counts the whole payload including itself — `0x14 + displays ×
/// 0x38`, the encoder's own arithmetic, and 132 for two screens or 76 for one — but
/// **two fewer bytes than that are actually sent**: the final display record stops
/// after its last field and omits its two trailing pad bytes. So a reader consumes
/// `declared - 2`, and consuming `declared` swallows the first two bytes of the
/// message behind it. That desync is unrecoverable and does not look like a length
/// bug: the next thing read is a framebuffer update whose rectangle count is really
/// a screen width, and the session dies several messages later complaining about an
/// encoding nobody sent. Measured against macOS 26 twice over, once for a
/// two-screen layout and once for a one-screen one.
pub fn parse_layout(payload: &[u8]) -> anyhow::Result<Layout> {
    anyhow::ensure!(
        payload.len() >= LAYOUT_HEAD,
        "a display layout carried {} bytes, too few for a header",
        payload.len()
    );
    let declared = usize::from(be16(payload, 0));
    anyhow::ensure!(
        declared == payload.len() + 2,
        "a display layout says {declared} bytes and carried {}, which is not the {} expected",
        payload.len(),
        declared.saturating_sub(2)
    );
    let version = be16(payload, 2);
    // Plus the two the last record does not send, so the grid divides.
    let records = payload.len() - LAYOUT_HEAD + 2;
    anyhow::ensure!(
        records.is_multiple_of(LAYOUT_RECORD),
        "a display layout has {records} bytes of records, not a multiple of {LAYOUT_RECORD}"
    );
    // `0xffffffff` means the combined view of every screen. Any other value is a
    // screen id, and it is how a selection is confirmed — the gateway believes it
    // acted only when this comes back changed.
    let current = match be32(payload, 0x0c) {
        u32::MAX => None,
        id => Some(id),
    };
    // Logged whole, because the word at `+0x10` is still unidentified — it read 4
    // on every layout of every measured session, selected or combined — and this is
    // the only way anyone will identify it.
    debug!(
        "vnc: display layout version {version}, {} display(s), current {current:?}, header {:02x?}",
        records / LAYOUT_RECORD,
        &payload[..LAYOUT_HEAD]
    );

    let mut displays = Vec::new();
    // `chunks`, not `chunks_exact`: the last record is two bytes short. Every field
    // read below ends at 0x2a, so a short final chunk still holds all of them —
    // asserted rather than assumed, since a future build that truncates further
    // would otherwise read a neighbouring record's bytes as this one's flags.
    for (index, record) in payload[LAYOUT_HEAD..].chunks(LAYOUT_RECORD).enumerate() {
        anyhow::ensure!(
            record.len() >= LAYOUT_FIELDS,
            "a display layout's record {index} carried {} bytes, too few for its fields",
            record.len()
        );
        let flags = be32(record, 0x26);
        // A mirrored screen shows another screen's pixels. Offering it would be
        // offering the same picture twice under two names.
        if flags & 0x02 != 0 {
            continue;
        }
        let edges = |at: usize| {
            let (top, left, bottom, right) = (
                be16(record, at),
                be16(record, at + 2),
                be16(record, at + 4),
                be16(record, at + 6),
            );
            (right.saturating_sub(left), bottom.saturating_sub(top))
        };
        let logical = edges(0x16);
        let backing = edges(0x1e);
        // An unusable screen is dropped, the way a mirrored one is, rather than
        // taking the whole layout with it. One odd record among good ones would
        // otherwise cost the entire display list *and* the resize — and a layout
        // arrives at every login and lock, so that is a session-long outage over one
        // screen. If every record is unusable the emptiness check below still fails
        // loudly, which is what a wrong set of offsets looks like.
        if logical.0 == 0 || logical.1 == 0 {
            warn!(
                "vnc: display layout record {index} is {}x{} points; not offering it",
                logical.0, logical.1
            );
            continue;
        }
        // The Mac states the density twice over — once as a double, once as the
        // ratio of the two rects. Taking the double and checking the ratio means a
        // build that disagrees with itself says so, instead of quietly halving a
        // desktop.
        let stated = f64::from_be_bytes(
            record[0x02..0x0a].try_into().expect("eight bytes inside a 0x38-byte record"),
        );
        if !stated.is_finite() || !(1.0..=4.0).contains(&stated) {
            warn!(
                "vnc: display layout record {index} states a scale factor of {stated}, \
                 outside 1..=4; not offering it"
            );
            continue;
        }
        let density = stated as f32;
        let ratio = f32::from(backing.0) / f32::from(logical.0);
        if (ratio - density).abs() > 0.01 {
            warn!(
                "vnc: display {} states scale {density} but its rects give {ratio}; \
                 using the stated one",
                be32(record, 0x12)
            );
        }
        // "1600×900 at 2x" — the points a window occupies, which is the size a
        // person recognises, and then the density that earns it more pixels.
        // `f32`'s own Display gives "2" for 2.0 and "1.5" for 1.5, which is exactly
        // the two shapes wanted and neither of them "2.0x".
        let suffix = if density > 1.005 { format!(" at {density}x") } else { String::new() };
        displays.push(Display {
            info: DisplayInfo {
                id: be32(record, 0x12),
                label: format!("Display {}", index + 1),
                detail: format!("{}×{}{suffix}", logical.0, logical.1),
                main: flags & 0x01 != 0,
                // Never: this client asks for the Mac's own screens and does not
                // ask it to make one. Asking is precisely what
                // `SetDisplayConfiguration` did, which is why it is no longer sent
                // — see [`crate::vnc`].
                virtual_display: false,
            },
            density,
            backing,
        });
    }

    anyhow::ensure!(!displays.is_empty(), "a display layout listed no usable display");
    let backing = (be16(payload, 0x08), be16(payload, 0x0a));
    anyhow::ensure!(
        backing.0 > 0 && backing.1 > 0,
        "a display layout gives a {}x{} framebuffer",
        backing.0,
        backing.1
    );
    Ok(Layout { backing, current, displays })
}

/// The connection's zlib inflate stream.
///
/// One deflate stream for the life of the connection, chunked across rectangles:
/// the sliding window carries over, so this context is created once and never
/// reset. A fresh one per rectangle decodes the first rectangle and then fails —
/// or, worse, succeeds with the wrong pixels.
pub struct ZlibStream {
    inflate: flate2::Decompress,
    what: &'static str,
}

impl ZlibStream {
    pub fn new(what: &'static str) -> Self {
        Self {
            inflate: flate2::Decompress::new(true),
            what,
        }
    }

    /// Inflate one chunk to exactly `expect` bytes.
    ///
    /// `expect` is known from the geometry the rectangle already declared, so a
    /// payload that wants to expand past it is a protocol violation rather than a
    /// buffer to grow — which is also what keeps a compression bomb from being
    /// answered with memory.
    pub fn inflate(&mut self, chunk: &[u8], expect: usize) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            expect <= MAX_INFLATED,
            "a {} rectangle wants {expect} inflated bytes, past the {MAX_INFLATED} ceiling",
            self.what
        );
        let mut out = Vec::with_capacity(expect);
        let mut fed = 0;
        while fed < chunk.len() && out.len() < expect {
            let before = (self.inflate.total_in(), self.inflate.total_out());
            self.inflate
                .decompress_vec(&chunk[fed..], &mut out, flate2::FlushDecompress::Sync)
                .with_context(|| format!("inflating a {} rectangle", self.what))?;
            fed += (self.inflate.total_in() - before.0) as usize;
            if (self.inflate.total_in(), self.inflate.total_out()) == before {
                // Neither side moved, so feeding more of the same chunk cannot
                // help: either the stream wants output space this rectangle does
                // not claim, or it is truncated.
                break;
            }
        }
        anyhow::ensure!(
            out.len() == expect,
            "a {} rectangle inflated to {} bytes, not the {expect} its geometry claims",
            self.what,
            out.len()
        );
        Ok(out)
    }
}

/// Apple's cursor shapes, which arrive as a cache rather than as pixels.
///
/// A shape is sent once with pixels (*store*) and then re-selected by id every
/// time the pointer changes over an I-beam or a resize handle (*select*), which in
/// a steady session is nearly all of them. So the pixels have to be kept: a select
/// for an id that was never stored is the one case with nothing to draw, and the
/// honest answer there is to leave the last shape alone rather than blank the
/// pointer.
#[derive(Default)]
pub struct CursorCache {
    shapes: HashMap<u32, CursorShape>,
    zlib: Option<ZlibStream>,
}

/// What a cursor rectangle asked for.
#[derive(Debug)]
pub enum Cursor {
    /// Draw this shape.
    Shape(CursorShape),
    /// A select for an id that was never stored: leave the pointer as it is, which
    /// is closer to the truth than blanking it.
    ///
    /// There is no "the server hid the pointer" here. On the plain RFB Cursor
    /// pseudo-encoding a zero-sized rectangle means exactly that, but this encoding
    /// never says how it is spelled, and treating a zero-sized *store* as hidden
    /// meant discarding a chunk of the shared deflate stream — see
    /// [`CursorCache::accept`].
    Unchanged,
}

impl CursorCache {
    /// Apply one `CursorImage` rectangle. `body` is everything after the
    /// `cache_id` and `compressed_len` words; `hotspot` and `size` come from the
    /// rectangle header, which the encoding repurposes for them.
    pub fn accept(
        &mut self,
        id: u32,
        (hx, hy): (u16, u16),
        (w, h): (u16, u16),
        deflated: &[u8],
    ) -> anyhow::Result<Cursor> {
        if deflated.is_empty() {
            // A select. Its geometry fields are zeroed, so the cache is the only
            // thing that knows the shape.
            return Ok(match self.shapes.get(&id) {
                Some(shape) => Cursor::Shape(shape.clone()),
                None => {
                    debug!("vnc: cursor select for unknown cache id {id}");
                    Cursor::Unchanged
                }
            });
        }
        // A store this client cannot decode cannot be *skipped* either. Its payload
        // is a chunk of the connection's single deflate stream, so not feeding it
        // leaves that stream one chunk behind for the rest of the session and every
        // later shape inflates to rubbish — silently, since rubbish of the right
        // length still parses. Nor can it be fed and discarded: a zero-dimension
        // store gives no expected size to inflate to, and an oversized one gives a
        // size up to 21 GB. So both are fatal, which for shapes a real Mac never
        // sends costs nothing and removes the one path that could corrupt the rest.
        anyhow::ensure!(
            w != 0 && h != 0,
            "a cursor store carried {} compressed bytes for a {w}x{h} shape",
            deflated.len()
        );
        anyhow::ensure!(
            w <= MAX_CURSOR_DIM && h <= MAX_CURSOR_DIM,
            "a cursor store is {w}x{h}, past the {MAX_CURSOR_DIM}-pixel edge this client draws"
        );

        let pixels = usize::from(w) * usize::from(h);
        let stream = self
            .zlib
            .get_or_insert_with(|| ZlibStream::new("cursor image"));
        let raw = stream.inflate(deflated, pixels * 4 + pixels)?;
        // BGRA pixels, then a *separate* alpha plane. The fourth byte of each
        // pixel is not the alpha — folding it in is how a cursor comes out
        // uniformly opaque or invisible.
        let (bgrx, alpha) = raw.split_at(pixels * 4);
        let mut rgba = Vec::with_capacity(pixels * 4);
        for (px, &a) in bgrx.chunks_exact(4).zip(alpha) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], a]);
        }
        let shape = CursorShape::from_rgba(w, h, hx, hy, &rgba)?;
        debug!("vnc: cursor {w}x{h} stored as {id}, {} bytes", shape.png.len());
        self.shapes.insert(id, shape.clone());
        Ok(Cursor::Shape(shape))
    }
}

/// One display as a test writes it: id, logical size, backing size, flags.
#[cfg(test)]
pub(crate) type TestScreen = (u32, (u16, u16), (u16, u16), u32);

/// Build a layout payload at the *measured* offsets, for the cases the captured
/// bytes in this module's tests cannot cover — a mirrored screen, a selection, a
/// malformed length.
///
/// Lives outside `mod tests` because [`crate::vnc`]'s tests need it too, and two
/// copies of this bit-twiddling would have to be kept in step with the parser by
/// hand. Cross-checked against the capture by
/// `the_builder_agrees_with_the_captured_bytes`, so a drift between builder and
/// parser fails a test rather than agreeing with itself.
#[cfg(test)]
pub(crate) fn test_layout(current: Option<u32>, displays: &[TestScreen]) -> Vec<u8> {
    let mut payload = vec![0u8; LAYOUT_HEAD];
    let total = LAYOUT_HEAD + displays.len() * LAYOUT_RECORD;
    payload[..2].copy_from_slice(&u16::try_from(total).unwrap().to_be_bytes());
    payload[2..4].copy_from_slice(&5u16.to_be_bytes());
    let first = displays.first().expect("at least one display");
    // The header's logical geometry spans every screen and does not move.
    let span: u16 = displays.iter().map(|d| d.1.0).sum();
    // Its backing is the framebuffer: that screen's own pixels when one is selected,
    // and the union of every screen's when none is — which is what makes a
    // mixed-density Mac's framebuffer wider than any single scale explains.
    let framebuffer = current
        .and_then(|id| displays.iter().find(|d| d.0 == id))
        .map_or_else(
            || {
                (
                    displays.iter().map(|d| d.2.0).sum(),
                    displays.iter().map(|d| d.2.1).max().unwrap_or(0),
                )
            },
            |d| d.2,
        );
    payload[4..6].copy_from_slice(&span.to_be_bytes());
    payload[6..8].copy_from_slice(&first.1.1.to_be_bytes());
    payload[8..10].copy_from_slice(&framebuffer.0.to_be_bytes());
    payload[10..12].copy_from_slice(&framebuffer.1.to_be_bytes());
    payload[12..16].copy_from_slice(&current.unwrap_or(u32::MAX).to_be_bytes());
    payload[16..20].copy_from_slice(&4u32.to_be_bytes());
    let mut left = 0u16;
    for (id, logical, backing, flags) in displays {
        let mut r = vec![0u8; LAYOUT_RECORD];
        let density = f64::from(backing.0) / f64::from(logical.0.max(1));
        r[0x02..0x0a].copy_from_slice(&density.to_be_bytes());
        r[0x0a..0x12].copy_from_slice(&1.0f64.to_be_bytes());
        r[0x12..0x16].copy_from_slice(&id.to_be_bytes());
        // (top, left, bottom, right), laid out left to right.
        let edges = |at: usize, r: &mut [u8], w: u16, h: u16, x: u16| {
            r[at..at + 2].copy_from_slice(&0u16.to_be_bytes());
            r[at + 2..at + 4].copy_from_slice(&x.to_be_bytes());
            r[at + 4..at + 6].copy_from_slice(&h.to_be_bytes());
            r[at + 6..at + 8].copy_from_slice(&(x + w).to_be_bytes());
        };
        edges(0x16, &mut r, logical.0, logical.1, left);
        edges(0x1e, &mut r, backing.0, backing.1, left);
        r[0x26..0x2a].copy_from_slice(&flags.to_be_bytes());
        payload.extend_from_slice(&r);
        left += logical.0;
    }
    // The Mac stops two bytes short of the last record, and `declared` counts them
    // anyway. A builder that did not do this would let the parser's length rule drift
    // without a single test noticing.
    payload.truncate(payload.len() - 2);
    payload
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_encryption_is_the_observed_bytes() {
        assert_eq!(
            set_encryption_start(),
            vec![0x12, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01]
        );
        assert_eq!(
            set_encryption_stop(),
            vec![0x12, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn arming_and_display_selection_are_fixed_shapes() {
        let arm = auto_framebuffer_update((3840, 2160));
        assert_eq!(arm.len(), 16);
        assert_eq!(arm[0], 0x09);
        assert_eq!(be16(&arm, 2), 1);
        assert_eq!(be32(&arm, 4), 0, "server-default update interval");
        assert_eq!(be16(&arm, 12), 3840);
        assert_eq!(be16(&arm, 14), 2160);

        let pick = set_display_message(Some(0x2b00_4501));
        assert_eq!(pick, vec![0x0d, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x45, 0x01]);
        assert_eq!(pick[1], 0, "one named screen");

        let all = set_display_message(None);
        assert_eq!(all, vec![0x0d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    /// The second `SetEncodings` is the first one with zlib appended, and nothing
    /// else about it moved.
    ///
    /// The two lists are written out longhand rather than derived, because the first
    /// is a measured constant and building the second from it would invite someone to
    /// build the *first* from something too. That leaves them free to drift — an
    /// entry dropped, or the order changed in one and not the other — and drift here
    /// costs the display layout silently, with a session that still connects and
    /// paints. This is the check that a reordering cannot pass.
    #[test]
    fn the_zlib_list_is_the_first_list_plus_zlib() {
        assert_eq!(
            ENCODINGS_WITH_ZLIB.len(),
            ENCODINGS.len() + 1,
            "exactly one entry more"
        );
        assert_eq!(
            &ENCODINGS_WITH_ZLIB[..ENCODINGS.len()],
            ENCODINGS,
            "the same entries in the same order, which is the part that matters"
        );
        assert_eq!(
            ENCODINGS_WITH_ZLIB.last(),
            Some(&ENCODING_ZLIB),
            "and zlib on the end"
        );
        assert!(
            !ENCODINGS.contains(&ENCODING_ZLIB),
            "zlib in the first list is what costs the display layout"
        );
    }

    /// The `AppleDisplayLayout` a macOS 26 VM sent for its two real screens, byte
    /// for byte off the wire (see docs/apple-vnc-889.md).
    ///
    /// Captured rather than constructed, because a payload this parser built for
    /// itself would agree with whichever offsets it happened to use — which is
    /// exactly how the offsets came to be two bytes out. The ground truth these
    /// bytes have to reproduce was measured separately, over SSH: display ids 1 and
    /// 4, a 1280x800 at (0,0) and a 1600x900 at (1280,0), the first one main, and
    /// the second one Retina.
    const TWO_REAL_SCREENS: &[u8] = &[
        // header: len 132, version 5, logical 2880x900, backing 4480x1800,
        // current_display 0xffffffff (the combined view), then the unidentified word.
        0x00, 0x84, 0x00, 0x05, 0x0b, 0x40, 0x03, 0x84, 0x11, 0x80, 0x07, 0x08, 0xff, 0xff, 0xff,
        0xff, 0x00, 0x00, 0x00, 0x04, //
        // display id 1: scale 1.0, viewer scale 1.0, both rects (0,0,800,1280), main.
        0x00, 0x02, 0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0xf0, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x03, 0x20, 0x05, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x03, 0x20, 0x05, 0x00, 0x00, 0x00, 0x00, 0x01, 0x20, 0x20, 0x00,
        0x01, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff, 0x10, 0x08, 0x00, 0x00, //
        // display id 4: scale 2.0, logical (0,1280,900,2880), backing (0,1280,1800,4480).
        0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0xf0, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x05, 0x00, 0x03, 0x84, 0x0b, 0x40,
        0x00, 0x00, 0x05, 0x00, 0x07, 0x08, 0x11, 0x80, 0x00, 0x00, 0x00, 0x00, 0x20, 0x20, 0x00,
        0x01, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff, 0x10, 0x08,
        // …and here it stops. The last record's two trailing pad bytes are not on
        // the wire, which is why the prefix above reads 132 for 130 bytes.
    ];

    #[test]
    fn a_captured_layout_reproduces_the_macs_real_screens() {
        let parsed = parse_layout(TWO_REAL_SCREENS).unwrap();

        assert_eq!(parsed.backing, (4480, 1800), "the framebuffer, from the header");
        assert_eq!(parsed.current, None, "0xffffffff is the combined view");
        assert_eq!(parsed.displays.len(), 2);
        assert_eq!(
            parsed.repaint_pixels(),
            1280 * 800 + 3200 * 1800,
            "the gap below the shorter display is not part of a full paint"
        );

        let main = &parsed.displays[0];
        assert_eq!(main.info.id, 1, "the CGDirectDisplayID SSH also reported");
        assert_eq!(main.info.label, "Display 1");
        assert_eq!(main.info.detail, "1280×800", "no suffix on a 1x screen");
        assert_eq!(main.density, 1.0);
        assert!(main.info.main);
        assert!(!main.info.virtual_display);

        let retina = &parsed.displays[1];
        assert_eq!(retina.info.id, 4);
        assert_eq!(retina.info.label, "Display 2");
        assert_eq!(retina.info.detail, "1600×900 at 2x", "points, then the density");
        assert_eq!(retina.density, 2.0);
        assert!(!retina.info.main);
    }

    #[test]
    fn the_scale_is_the_selected_screens_own() {
        // Combined: no one scale is true of a 1x screen beside a 2x one, and the
        // ratio of the header's own two geometries (4480/2880) is the meaningless
        // number that reading it would produce.
        let combined = parse_layout(TWO_REAL_SCREENS).unwrap();
        assert_eq!(combined.scale(), crate::protocol::UNSCALED);

        // Selecting one screen makes it exact, which is the whole reason picking
        // matters. Both edits are what the Mac actually answered a `0x0d` with: the
        // framebuffer narrows to that screen and its id lands in `current_display`.
        let mut payload = TWO_REAL_SCREENS.to_vec();
        payload[0x08..0x0a].copy_from_slice(&3200u16.to_be_bytes());
        payload[0x0c..0x10].copy_from_slice(&4u32.to_be_bytes());
        let retina = parse_layout(&payload).unwrap();
        assert_eq!(retina.current, Some(4));
        assert_eq!(retina.backing, (3200, 1800));
        assert_eq!(retina.scale(), 2.0);
        assert_eq!(retina.repaint_pixels(), 3200 * 1800);

        payload[0x08..0x0a].copy_from_slice(&1280u16.to_be_bytes());
        payload[0x0a..0x0c].copy_from_slice(&800u16.to_be_bytes());
        payload[0x0c..0x10].copy_from_slice(&1u32.to_be_bytes());
        let plain = parse_layout(&payload).unwrap();
        assert_eq!(plain.scale(), 1.0);
        assert_eq!(plain.backing, (1280, 800));

        // A screen that is gone by the time the id is looked up leaves the desktop
        // at its pixel size rather than guessing at another screen's density.
        payload[0x0c..0x10].copy_from_slice(&99u32.to_be_bytes());
        assert_eq!(parse_layout(&payload).unwrap().scale(), crate::protocol::UNSCALED);
    }

    /// The shared builder, which lives outside this module so [`crate::vnc`]'s tests
    /// can use the same one. See [`test_layout`].
    use super::test_layout as layout;

    #[test]
    fn the_builder_agrees_with_the_captured_bytes() {
        let built = layout(
            None,
            &[(1, (1280, 800), (1280, 800), 0x01), (4, (1600, 900), (3200, 1800), 0x00)],
        );
        // Not byte-equal: the capture carries a leading `u16` per record and a
        // pixel-format tail that nothing here reads. Equal in every field the parser
        // does read, which is the claim that matters.
        assert_eq!(parse_layout(&built).unwrap(), parse_layout(TWO_REAL_SCREENS).unwrap());
    }

    #[test]
    fn a_mirrored_screen_is_not_offered_twice() {
        let parsed = parse_layout(&layout(
            None,
            &[(11, (1920, 1080), (1920, 1080), 0x01), (22, (1920, 1080), (1920, 1080), 0x02)],
        ))
        .unwrap();
        assert_eq!(parsed.displays.len(), 1);
        assert_eq!(parsed.displays[0].info.id, 11);
        // One entry is what makes both clients hide the picker, which is right:
        // there is nothing to choose.
    }

    #[test]
    fn a_layout_that_does_not_add_up_is_refused() {
        let one = |flags| layout(None, &[(11, (800, 600), (800, 600), flags)]);

        // Short of a header.
        assert!(parse_layout(&[0, 4, 0, 5]).is_err());

        // A payload as long as its own prefix claims — which is a *shorter* payload
        // than the Mac sends, and exactly the mistake this parser used to make. Named
        // here because reading it silently was what desynced the session.
        let mut payload = one(0x01);
        let exact = payload.len();
        payload[..2].copy_from_slice(&u16::try_from(exact).unwrap().to_be_bytes());
        let err = parse_layout(&payload).unwrap_err();
        assert!(format!("{err:#}").contains("and carried"), "{err:#}");

        // A trailing partial record: eight bytes more than any whole number of them.
        let mut payload = one(0x01);
        payload.extend_from_slice(&[0u8; 8]);
        let declared = payload.len() + 2;
        payload[..2].copy_from_slice(&u16::try_from(declared).unwrap().to_be_bytes());
        let err = parse_layout(&payload).unwrap_err();
        assert!(format!("{err:#}").contains("not a multiple of"), "{err:#}");

        // Every screen mirrored leaves nothing to render.
        let err = parse_layout(&one(0x02)).unwrap_err();
        assert!(format!("{err:#}").contains("no usable display"), "{err:#}");

        // A scale factor read out of the wrong offset, which is what the reference's
        // own field model produces: the bytes there are a denormal, not a density.
        // The record is dropped rather than the layout refused, so with only one
        // screen in it what is left is nothing to render — and a *wrong set of
        // offsets* fails exactly here, because it would drop every record.
        let mut payload = one(0x01);
        payload[LAYOUT_HEAD + 0x02..LAYOUT_HEAD + 0x0a]
            .copy_from_slice(&[0, 0, 0x3f, 0xf0, 0, 0, 0, 0]);
        let err = parse_layout(&payload).unwrap_err();
        assert!(format!("{err:#}").contains("no usable display"), "{err:#}");
    }

    /// One unusable screen costs that screen and not the layout.
    ///
    /// A display list arrives at every login and lock, so refusing the whole payload
    /// over one odd record would take out the picker and the resize for the rest of
    /// the session. Mirrored screens have always been dropped this way; a bogus scale
    /// factor and a zero-sized rect now are too.
    #[test]
    fn one_unusable_screen_does_not_cost_the_others() {
        let screens: [TestScreen; 3] = [
            (11, (1920, 1080), (1920, 1080), 0x01),
            (22, (1600, 900), (3200, 1800), 0x00),
            (33, (1280, 800), (1280, 800), 0x00),
        ];

        // A scale factor no screen has.
        let mut payload = layout(None, &screens);
        payload[LAYOUT_HEAD + 0x02..LAYOUT_HEAD + 0x0a].copy_from_slice(&99.0f64.to_be_bytes());
        let parsed = parse_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 2, "the other two are still offered");
        assert_eq!(parsed.displays[0].info.id, 22);
        assert_eq!(parsed.displays[1].info.id, 33);

        // A screen of no size, which would otherwise be offered as "0×0".
        let mut payload = layout(None, &screens);
        let second = LAYOUT_HEAD + LAYOUT_RECORD;
        payload[second + 0x16..second + 0x1e].copy_from_slice(&[0u8; 8]);
        let parsed = parse_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 2);
        assert!(parsed.displays.iter().all(|d| d.info.id != 22));
    }

    /// One stream across rectangles, which is the rule that makes zlib usable at
    /// all here. Two chunks of one deflate stream inflate in sequence and fail
    /// separately.
    #[test]
    fn zlib_inflates_across_rectangles_but_not_out_of_order() {
        use flate2::{Compress, Compression, FlushCompress};

        let first = vec![0xabu8; 4096];
        let second = vec![0xcdu8; 4096];
        let mut deflate = Compress::new(Compression::default(), true);
        let chunk = |deflate: &mut Compress, raw: &[u8]| {
            let mut out = Vec::with_capacity(raw.len());
            let mut fed = 0;
            while fed < raw.len() {
                let before = deflate.total_in();
                out.reserve(raw.len());
                deflate
                    .compress_vec(&raw[fed..], &mut out, FlushCompress::Sync)
                    .unwrap();
                fed += (deflate.total_in() - before) as usize;
            }
            out
        };
        let a = chunk(&mut deflate, &first);
        let b = chunk(&mut deflate, &second);

        let mut stream = ZlibStream::new("test");
        assert_eq!(stream.inflate(&a, first.len()).unwrap(), first);
        assert_eq!(stream.inflate(&b, second.len()).unwrap(), second);

        // The second chunk alone, through a stream that never saw the first: no
        // zlib header, no window, nothing.
        let mut fresh = ZlibStream::new("test");
        assert!(fresh.inflate(&b, second.len()).is_err());

        // A chunk that does not inflate to the size its geometry claims.
        let mut stream = ZlibStream::new("test");
        let err = stream.inflate(&a, first.len() + 1).unwrap_err();
        assert!(format!("{err:#}").contains("its geometry claims"), "{err:#}");
    }

    /// Store once, then select by id — which is nearly every cursor change in a
    /// live session, and the reason the pixels have to be kept.
    #[test]
    fn a_cursor_is_stored_once_and_selected_by_id() {
        use flate2::{Compress, Compression, FlushCompress};

        let (w, h) = (2u16, 2u16);
        let pixels = usize::from(w) * usize::from(h);
        let mut raw = Vec::new();
        for i in 0..pixels {
            // BGRA, whose fourth byte is deliberately *not* the alpha.
            raw.extend_from_slice(&[10 + i as u8, 20, 30, 0xff]);
        }
        raw.extend_from_slice(&[0x00, 0x40, 0x80, 0xff]); // the real alpha plane

        let mut deflate = Compress::new(Compression::default(), true);
        let mut deflated = Vec::with_capacity(raw.len());
        deflate
            .compress_vec(&raw, &mut deflated, FlushCompress::Sync)
            .unwrap();

        let mut cache = CursorCache::default();
        let stored = cache.accept(1000, (1, 1), (w, h), &deflated).unwrap();
        let png = match stored {
            Cursor::Shape(shape) => {
                assert_eq!((shape.w, shape.h, shape.hx, shape.hy), (2, 2, 1, 1));
                shape.png
            }
            _ => panic!("a store should produce a shape"),
        };

        // A select carries no pixels and zeroed geometry, and must reproduce it.
        match cache.accept(1000, (0, 0), (0, 0), &[]).unwrap() {
            Cursor::Shape(shape) => assert_eq!(shape.png, png),
            _ => panic!("a select for a stored id should reproduce it"),
        }

        // A select for an id that was never stored leaves the pointer alone rather
        // than blanking it.
        assert!(matches!(
            cache.accept(1001, (0, 0), (0, 0), &[]).unwrap(),
            Cursor::Unchanged
        ));

        // A store this client cannot decode is fatal rather than skipped: its
        // payload belongs to the shared deflate stream, and dropping it would leave
        // every later shape inflating to rubbish of plausible length.
        let err = cache.accept(1002, (0, 0), (0, 0), &deflated).unwrap_err();
        assert!(format!("{err:#}").contains("for a 0x0 shape"), "{err:#}");
        let err = cache
            .accept(1003, (0, 0), (MAX_CURSOR_DIM + 1, 32), &deflated)
            .unwrap_err();
        assert!(format!("{err:#}").contains("past the"), "{err:#}");
    }
}
