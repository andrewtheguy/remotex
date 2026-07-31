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
//! **Not** picking a screen, which is what this wire was reached for. A 003.889
//! session makes macOS 26 synthesize one display and remove the Mac's real ones
//! until it ends, so no [`ENCODING_DISPLAY_LAYOUT`] ever arrives and
//! [`set_display_message`] is ignored — there is only ever one display to pick.
//! Both are still implemented and unit-tested against the reference's field model,
//! and both are dormant. Do not read their presence as evidence that picking works;
//! docs/apple-vnc-889.md has the measurements.
//!
//! ## What is deliberately absent
//!
//! No virtual display and no dynamic resolution: [`set_display_configuration`]
//! sends the *static* descriptor, which asks for the Mac's real screens at the
//! size they are set to. No Adaptive media path (HEVC/AAC over SRTP), so
//! `RFBMediaStreamMessage1` is never advertised. No pasteboard: on this wire the
//! Mac carries the clipboard over messages of its own rather than RFB's, so
//! `clipboard` is refused for this subtype at config load.
//!
//! ## Reading the offsets in here
//!
//! The reference document is reverse-engineered and says so; a few offsets in it
//! disagree with the bytes a live Mac sends, and where they do the comments name
//! which reading is implemented and why. Payload hex is logged at `debug` on
//! first receipt for exactly that reason.

use std::collections::HashMap;

use anyhow::Context as _;
use log::debug;

use crate::protocol::{CursorShape, DisplayInfo};

/// Raw pixels, the standard RFB encoding, still the fallback here.
const ENCODING_RAW: i32 = 0;
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
/// The Mac's model and enclosure colour. Parsed and dropped.
pub const ENCODING_DEVICE_INFO: i32 = 0x456;

/// What this client advertises in `SetEncodings` on the 003.889 wire.
///
/// Every entry is decoded, which is a requirement and not a courtesy: a server
/// takes the list as a promise and will send what it finds there. That is why
/// Apple's own still-image codecs are absent — the reference leaves their payload
/// formats unresolved, so advertising them would ask for rectangles this client
/// could only guess at.
///
/// The metadata encodings are advertised even though their contents are dropped,
/// because *skipping* a rectangle still means knowing its length: an
/// unadvertised one would arrive anyway on some builds and there would be no way
/// to step over it.
pub const ENCODINGS: &[i32] = &[
    ENCODING_ZLIB,
    ENCODING_RAW,
    ENCODING_CURSOR_IMAGE,
    ENCODING_DISPLAY_LAYOUT,
    ENCODING_VENDOR_KEYSYMS,
    ENCODING_KEYBOARD_SOURCE,
    ENCODING_DEVICE_INFO,
];

/// Bytes of one entry in a display configuration's mode table.
const MODE_ENTRY: usize = 0x1c;
/// Bytes of a display descriptor before its mode table.
const DESCRIPTOR_HEAD: usize = 0x9c;
/// Bytes of a display configuration before its first descriptor.
const CONFIG_HEAD: usize = 0x0c;
/// Bytes of one display record in a layout payload.
const LAYOUT_RECORD: usize = 0x38;
/// Bytes of a layout payload before its first record, *including* the `u16`
/// length prefix — see [`parse_layout`] for why that is the reading used.
const LAYOUT_HEAD: usize = 0x14;
/// Dots per inch the physical size in a descriptor is derived at. Not a real
/// measurement of anything: it is the figure that reproduces the sizes native
/// Screen Sharing sends for a 1920x1080 display, and the server only passes it
/// through to a display it is not being asked to create.
const NOMINAL_DPI: f32 = 132.0;
/// Largest cursor edge accepted, matching the plain RFB path. Real pointers are
/// 32x32 or 64x64.
const MAX_CURSOR_DIM: u16 = 256;
/// Ceiling on one inflated payload, so a hostile or broken stream cannot be
/// answered with unbounded memory.
const MAX_INFLATED: usize = 64 << 20;

/// `ViewerInfo`: who is connecting and what it can do.
///
/// **Not sent, and not sendable from this description.** Kept because working out
/// that it must not be sent cost a measurement, and the next reader deserves the
/// result rather than the search.
///
/// The reference frames the body as a version, an application id, *version
/// strings*, then a 32-byte capability bitmap — without saying how the strings are
/// framed. macOS 26 reads more bytes for the message than its own length field
/// declares, so any shape built from that description swallows whatever was sent
/// after it: the `SetEncryption` that should follow is consumed as the tail of this
/// message, the server waits for the rest of a message that has already gone, and
/// the rekey never arrives. The session hangs after ServerInit with no error from
/// either end.
///
/// Sending nothing works: the server rekeys on `SetEncryption` alone. The only bit
/// it is known to read out of a `ViewerInfo` gates observe-only mode, which this
/// client does not use, so there is nothing here worth recovering until the string
/// framing is known.
///
/// If a Mac ever *requires* it, this is the message to reconstruct, and the
/// symptom to expect is that same silent hang.
#[allow(dead_code)]
fn viewer_info_is_not_sent() {}

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

/// `SetDisplayConfiguration`: what kind of session this is.
///
/// The **static** descriptor — no dynamic-resolution flag, no virtual display,
/// zeroed mode indices — which asks for the Mac's real screens at the size they
/// are already set to. The alternative is a full dynamic descriptor, which makes
/// the Mac create a resizable virtual display and is what in-session resize is
/// built on; that is not implemented, and `resize` is refused for this subtype at
/// config load rather than accepted and ignored.
///
/// One mode entry, because the server rejects `mode_count = 0` and requires both
/// mode indices to be inside the table. It describes the size the session already
/// has, so it asks for nothing.
pub fn set_display_configuration((w, h): (u16, u16)) -> Vec<u8> {
    let descriptor = DESCRIPTOR_HEAD + MODE_ENTRY;
    let mut body = Vec::with_capacity(CONFIG_HEAD - 4 + descriptor);
    body.extend_from_slice(&1u16.to_be_bytes()); // version
    body.extend_from_slice(&1u16.to_be_bytes()); // display_count
    body.extend_from_slice(&0u32.to_be_bytes()); // flags

    let mut d = Vec::with_capacity(descriptor);
    d.extend_from_slice(&u16::try_from(descriptor).expect("descriptor within u16").to_be_bytes());
    d.resize(0x7a, 0); // the opaque 120-byte region, which may hold a name
    d.extend_from_slice(&0u32.to_be_bytes()); // display_flags: no dynamic resolution
    d.extend_from_slice(&0u32.to_be_bytes()); // display_type: not a virtual display
    let mm = |px: u16| (f32::from(px) / NOMINAL_DPI * 25.4).to_be_bytes();
    d.extend_from_slice(&mm(w));
    d.extend_from_slice(&mm(h));
    d.extend_from_slice(&u32::from(w).to_be_bytes()); // max_width
    d.extend_from_slice(&u32::from(h).to_be_bytes()); // max_height
    d.extend_from_slice(&0u16.to_be_bytes()); // current_mode_index
    d.extend_from_slice(&0u16.to_be_bytes()); // preferred_mode_index
    d.extend_from_slice(&0u32.to_be_bytes()); // rotations
    d.extend_from_slice(&1u16.to_be_bytes()); // mode_count
    debug_assert_eq!(d.len(), DESCRIPTOR_HEAD);
    for value in [w, h, w, h] {
        d.extend_from_slice(&u32::from(value).to_be_bytes());
    }
    d.extend_from_slice(&60.0f64.to_be_bytes()); // refresh_rate_hz
    d.extend_from_slice(&0u32.to_be_bytes()); // mode flags: not HDR
    debug_assert_eq!(d.len(), descriptor);

    body.extend_from_slice(&d);
    message(0x1d, &body)
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
    // "all/main displays" rather than a screen index: which screen is shared is
    // said with `set_display_message`, and pinning it twice invites the two to
    // disagree.
    msg.extend_from_slice(&u32::MAX.to_be_bytes());
    for value in [0, 0, w, h] {
        msg.extend_from_slice(&value.to_be_bytes());
    }
    msg
}

/// `SetDisplayMessage`: share this one display.
///
/// The `combine_all_displays` byte is zero throughout: the aggregate — every
/// screen in one framebuffer — is what a session already does without asking, so
/// this message exists here only to narrow it to one.
pub fn set_display_message(id: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8);
    msg.push(0x0d);
    msg.push(0); // combine_all_displays
    msg.extend_from_slice(&0u16.to_be_bytes()); // reserved
    msg.extend_from_slice(&id.to_be_bytes());
    msg
}

/// An Apple control message: type, a reserved byte, then the body's length and
/// the body. The length counts the body alone, which is the rule the server
/// bounds-checks and the easiest one to get wrong.
fn message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4 + body.len());
    msg.push(kind);
    msg.push(0);
    msg.extend_from_slice(&u16::try_from(body.len()).expect("body within u16").to_be_bytes());
    msg.extend_from_slice(body);
    msg
}

/// The Mac's display layout: what it is rendering, and what it could render
/// instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The framebuffer's size in pixels — what rectangles are addressed in.
    pub backing: (u16, u16),
    /// The same area in points. Half `backing` on a Retina screen, which is what
    /// a client needs in order to size a window rather than a canvas.
    pub logical: (u16, u16),
    pub displays: Vec<DisplayInfo>,
}

impl Layout {
    /// Pixels per point, for [`crate::protocol::ServerMsg::Resize`]. The
    /// framebuffer is `backing`; this says how large it should look.
    pub fn scale(&self) -> f32 {
        if self.logical.0 == 0 {
            return crate::protocol::UNSCALED;
        }
        f32::from(self.backing.0) / f32::from(self.logical.0)
    }
}

/// Parse an `AppleDisplayLayout` payload.
///
/// ## The length rule
///
/// The `u16` prefix counts the **whole payload including itself**, unlike the
/// other metadata encodings here where it counts what follows. That is the only
/// reading consistent with the encoder's own arithmetic
/// (`payload_len = displays × 0x38 + 0x14`) given a header the reference models as
/// 14 bytes: the six bytes it is missing are the geometry the live wire carries at
/// `+0x04`, and 14 + 6 is the 0x14 the encoder wrote. The record grid is
/// cross-checked against the end of the payload, so a build that disagrees fails
/// loudly here instead of silently shifting every field by two.
///
/// ## What is not read
///
/// The reference gives two conflicting readings of `+0x04`..`+0x14` — a
/// `current_display` word in one, the geometry in the other — and says to trust
/// the live bytes, which show the geometry. So the two words after it are left
/// alone rather than guessed at, and *which* display is being shared is tracked by
/// the gateway from what it asked for (see [`crate::vnc`]). That is no less
/// truthful: a request is only believed once the Mac has answered with a layout.
pub fn parse_layout(payload: &[u8]) -> anyhow::Result<Layout> {
    anyhow::ensure!(
        payload.len() >= LAYOUT_HEAD,
        "a display layout carried {} bytes, too few for a header",
        payload.len()
    );
    let declared = usize::from(be16(payload, 0));
    anyhow::ensure!(
        declared == payload.len(),
        "a display layout says {declared} bytes and carried {}",
        payload.len()
    );
    let version = be16(payload, 2);
    let records = payload.len() - LAYOUT_HEAD;
    anyhow::ensure!(
        records.is_multiple_of(LAYOUT_RECORD),
        "a display layout has {records} bytes of records, not a multiple of {LAYOUT_RECORD}"
    );
    // Logged whole, because two words of this header are unidentified and this is
    // the only way to identify them against a real Mac.
    debug!(
        "vnc: display layout version {version}, {} display(s), header {:02x?}",
        records / LAYOUT_RECORD,
        &payload[..LAYOUT_HEAD]
    );

    let mut displays = Vec::new();
    for (index, record) in payload[LAYOUT_HEAD..].chunks_exact(LAYOUT_RECORD).enumerate() {
        let flags = be32(record, 0x24);
        // A mirrored screen shows another screen's pixels. Offering it would be
        // offering the same picture twice under two names.
        if flags & 0x02 != 0 {
            continue;
        }
        let logical = (be16(record, 0x18), be16(record, 0x1a));
        let backing = (be16(record, 0x20), be16(record, 0x22));
        let density = if logical.0 != 0 && backing.0 / logical.0.max(1) >= 2 {
            format!(" at {}x", backing.0 / logical.0)
        } else {
            String::new()
        };
        displays.push(DisplayInfo {
            id: be32(record, 0x10),
            label: format!("Display {}", index + 1),
            detail: format!("{}×{}{density}", logical.0, logical.1),
            main: flags & 0x01 != 0,
            // Never: this client asks for the Mac's own screens and does not ask
            // it to make one.
            virtual_display: false,
        });
    }

    // The leading geometry is the first display's two rects, so a layout with no
    // usable record has no geometry either and there is nothing to render.
    anyhow::ensure!(!displays.is_empty(), "a display layout listed no usable display");
    Ok(Layout {
        logical: (be16(payload, 0x04), be16(payload, 0x06)),
        backing: (be16(payload, 0x08), be16(payload, 0x0a)),
        displays,
    })
}

/// How many bytes of payload follow the `u16` length prefix of a metadata
/// rectangle, and whether this client understands the encoding at all.
///
/// `VendorKeysymEncoding`, `KeyboardInputSource` and `DeviceInfo` all frame
/// themselves the same way — a `u16` saying how much comes after it — so one rule
/// steps over all three. Their contents describe the Mac's keyboard and model,
/// none of which this gateway acts on; what matters is walking past them by
/// exactly the right number of bytes, because the RFB stream above the record
/// layer has no framing of its own to resynchronise against.
pub fn metadata_remainder(prefix: u16) -> usize {
    usize::from(prefix)
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
pub enum Cursor {
    /// Draw this shape.
    Shape(CursorShape),
    /// A select for an id that was never stored, or a shape too large to draw:
    /// leave the pointer as it is.
    Unchanged,
    /// The server hid the pointer.
    Hidden,
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
        if w == 0 || h == 0 {
            return Ok(Cursor::Hidden);
        }
        if w > MAX_CURSOR_DIM || h > MAX_CURSOR_DIM {
            // The bytes still have to be consumed, and they have been — the caller
            // read the whole rectangle before calling. Only the shape is dropped.
            log::warn!("vnc: ignoring an oversized {w}x{h} cursor");
            return Ok(Cursor::Unchanged);
        }

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
    fn a_display_configuration_counts_its_lengths_the_way_the_server_does() {
        let msg = set_display_configuration((1920, 1080));
        assert_eq!(msg[0], 0x1d);
        // message_size counts everything after the 4-byte prefix.
        assert_eq!(usize::from(be16(&msg, 2)), msg.len() - 4);
        assert_eq!(msg.len(), CONFIG_HEAD + DESCRIPTOR_HEAD + MODE_ENTRY);
        assert_eq!(be16(&msg, 4), 1); // version
        assert_eq!(be16(&msg, 6), 1); // display_count

        let d = &msg[CONFIG_HEAD..];
        assert_eq!(usize::from(be16(d, 0)), DESCRIPTOR_HEAD + MODE_ENTRY);
        assert_eq!(be32(d, 0x7a), 0, "no dynamic-resolution flag");
        assert_eq!(be32(d, 0x7e), 0, "not a virtual display");
        assert_eq!(be32(d, 0x8a), 1920);
        assert_eq!(be32(d, 0x8e), 1080);
        assert_eq!(be16(d, 0x92), 0);
        assert_eq!(be16(d, 0x94), 0);
        assert_eq!(be16(d, 0x9a), 1, "mode_count must not be zero");
        assert_eq!(be32(d, 0x9c), 1920);
        assert_eq!(be32(d, 0xa0), 1080);
        assert_eq!(be32(d, 0xa4), 1920, "scaled_width equals width: no scaling");
        // 60.0 as a big-endian double, which the reference gives verbatim.
        assert_eq!(&d[0xac..0xb4], &[0x40, 0x4e, 0, 0, 0, 0, 0, 0]);

        // The physical size the reference observed for this resolution.
        let mm = |at: usize| f32::from_be_bytes(d[at..at + 4].try_into().unwrap());
        assert!((mm(0x82) - 369.45).abs() < 0.01, "{}", mm(0x82));
        assert!((mm(0x86) - 207.82).abs() < 0.01, "{}", mm(0x86));

        // And the same arithmetic at the reference's own worked example.
        let five = CONFIG_HEAD + DESCRIPTOR_HEAD + 5 * MODE_ENTRY;
        assert_eq!(five, 308);
        assert_eq!(five - 4, 304);
        assert_eq!(DESCRIPTOR_HEAD + 5 * MODE_ENTRY, 296);
    }

    #[test]
    fn arming_and_display_selection_are_fixed_shapes() {
        let arm = auto_framebuffer_update((3840, 2160));
        assert_eq!(arm.len(), 16);
        assert_eq!(arm[0], 0x09);
        assert_eq!(be16(&arm, 2), 1);
        assert_eq!(be32(&arm, 4), u32::MAX, "all/main displays");
        assert_eq!(be16(&arm, 12), 3840);
        assert_eq!(be16(&arm, 14), 2160);

        let pick = set_display_message(0x2b00_4501);
        assert_eq!(pick, vec![0x0d, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x45, 0x01]);
        assert_eq!(pick[1], 0, "never the combined aggregate");
    }

    /// One display as a test writes it: id, logical size, backing size, flags.
    type Screen = (u32, (u16, u16), (u16, u16), u32);

    /// Build a layout payload the way the encoder does, so the parser is read
    /// against the length relation rather than against itself.
    fn layout(displays: &[Screen]) -> Vec<u8> {
        let mut payload = vec![0u8; LAYOUT_HEAD];
        let total = LAYOUT_HEAD + displays.len() * LAYOUT_RECORD;
        payload[..2].copy_from_slice(&u16::try_from(total).unwrap().to_be_bytes());
        payload[2..4].copy_from_slice(&5u16.to_be_bytes());
        let first = displays.first().expect("at least one display");
        payload[4..6].copy_from_slice(&first.1.0.to_be_bytes());
        payload[6..8].copy_from_slice(&first.1.1.to_be_bytes());
        payload[8..10].copy_from_slice(&first.2.0.to_be_bytes());
        payload[10..12].copy_from_slice(&first.2.1.to_be_bytes());
        for (id, logical, backing, flags) in displays {
            let mut r = vec![0u8; LAYOUT_RECORD];
            r[0x08..0x10].copy_from_slice(&1.0f64.to_be_bytes());
            r[0x10..0x14].copy_from_slice(&id.to_be_bytes());
            r[0x18..0x1a].copy_from_slice(&logical.0.to_be_bytes());
            r[0x1a..0x1c].copy_from_slice(&logical.1.to_be_bytes());
            r[0x20..0x22].copy_from_slice(&backing.0.to_be_bytes());
            r[0x22..0x24].copy_from_slice(&backing.1.to_be_bytes());
            r[0x24..0x28].copy_from_slice(&flags.to_be_bytes());
            payload.extend_from_slice(&r);
        }
        payload
    }

    #[test]
    fn a_layout_becomes_the_display_list_a_client_shows() {
        let parsed = parse_layout(&layout(&[
            (0x2b00_4501, (1920, 1080), (3840, 2160), 0x01),
            (0x2b00_4502, (1600, 1000), (1600, 1000), 0x00),
        ]))
        .unwrap();

        assert_eq!(parsed.backing, (3840, 2160));
        assert_eq!(parsed.logical, (1920, 1080));
        assert_eq!(parsed.scale(), 2.0);
        assert_eq!(parsed.displays.len(), 2);

        let main = &parsed.displays[0];
        assert_eq!(main.id, 0x2b00_4501);
        assert_eq!(main.label, "Display 1");
        assert_eq!(main.detail, "1920×1080 at 2x");
        assert!(main.main);
        assert!(!main.virtual_display);

        let second = &parsed.displays[1];
        assert_eq!(second.label, "Display 2");
        assert_eq!(second.detail, "1600×1000", "no density suffix at 1x");
        assert!(!second.main);
    }

    #[test]
    fn a_mirrored_screen_is_not_offered_twice() {
        let parsed = parse_layout(&layout(&[
            (11, (1920, 1080), (1920, 1080), 0x01),
            (22, (1920, 1080), (1920, 1080), 0x02),
        ]))
        .unwrap();
        assert_eq!(parsed.displays.len(), 1);
        assert_eq!(parsed.displays[0].id, 11);
        // One entry is what makes both clients hide the picker, which is right:
        // there is nothing to choose.
    }

    #[test]
    fn a_layout_that_does_not_add_up_is_refused() {
        // Short of a header.
        assert!(parse_layout(&[0, 4, 0, 5]).is_err());

        // A declared length that disagrees with what arrived, which is the shape a
        // two-byte offset error would take.
        let mut payload = layout(&[(11, (800, 600), (800, 600), 0x01)]);
        let short = payload.len() - 2;
        payload[..2].copy_from_slice(&u16::try_from(short).unwrap().to_be_bytes());
        let err = parse_layout(&payload).unwrap_err();
        assert!(format!("{err:#}").contains("and carried"), "{err:#}");

        // A trailing partial record.
        let mut payload = layout(&[(11, (800, 600), (800, 600), 0x01)]);
        payload.extend_from_slice(&[0u8; 8]);
        let total = payload.len();
        payload[..2].copy_from_slice(&u16::try_from(total).unwrap().to_be_bytes());
        let err = parse_layout(&payload).unwrap_err();
        assert!(format!("{err:#}").contains("not a multiple of"), "{err:#}");

        // Every screen mirrored leaves nothing to render.
        let err = parse_layout(&layout(&[(11, (800, 600), (800, 600), 0x02)])).unwrap_err();
        assert!(format!("{err:#}").contains("no usable display"), "{err:#}");
    }

    #[test]
    fn the_metadata_encodings_share_one_length_rule() {
        // The reference's own vectors: a 22-byte vendor-keysym payload whose prefix
        // reads 0x14, and a keyboard-source payload of S + 10 whose prefix is S + 8.
        assert_eq!(metadata_remainder(0x0014) + 2, 22);
        let id = "com.apple.keylayout.ABC";
        let s = id.len();
        assert_eq!(metadata_remainder((s + 8) as u16) + 2, s + 10);
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

        // A zero-sized store is the server hiding the pointer.
        assert!(matches!(
            cache.accept(1002, (0, 0), (0, 0), &deflated).unwrap(),
            Cursor::Hidden
        ));
    }
}
