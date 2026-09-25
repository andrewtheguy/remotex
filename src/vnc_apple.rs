//! Apple's Screen Sharing messages and encodings. Both Apple subtypes speak RFB
//! 003.889 inside Apple's record layer ([`crate::vnc_record`]). `ard` is Standard
//! mode and uses the display, cursor, and pasteboard pieces for the Mac's physical
//! displays. `ard-high-performance` also requests a virtual display.
//!
//! Everything here is either a message this client builds or a rectangle payload
//! it parses. The transport is [`crate::vnc_record`]'s and the session loop is
//! [`crate::vnc`]'s; this module is pure and has no I/O, which is what lets the
//! wire formats be asserted byte for byte.
//!
//! ## What the extension is for, here
//!
//! **Compression**: standard zlib (`ENCODING_ZLIB`, decoded in
//! [`crate::vnc_encodings`]) instead of raw pixels, which is around fifty times
//! fewer bytes on a static desktop. Apple's private framebuffer codecs would do
//! better still, but their payload formats are unresolved in the reference this
//! was written from, and a client must not advertise an encoding it cannot decode.
//!
//! **Picking a physical screen in Standard mode**:
//! [`ENCODING_DISPLAY_LAYOUT`] carries the Mac's displays and
//! [`set_display_message`] binds one of them, narrowing the framebuffer to that
//! screen's own pixels.
//!
//! **The pixel density**, which comes with it. Each screen states both its native
//! density and the server-side scale Apple applied. Standard mode asks the Mac to
//! scale the framebuffer to the browser's display density, so a 2x 2880x1800
//! screen viewed from a 1x display arrives as 1440x900 instead of being shrunk in
//! the browser. Because density is per screen, the *combined* view of a
//! mixed-density Mac has no single scale; it is composed in the browser from the
//! regions [`Layout::mosaic`] names, as Apple's viewer composes it.
//!
//! **A virtual display in High Performance mode**:
//! [`set_display_configuration`] asks for one virtual display at the configured
//! mode. When the target allows resize, a viewport report sends the same message
//! with a replacement mode. The dynamic-resolution flag is set on every message,
//! including setup, so a new connection always restores that Mac checkbox to on.
//! The one/two-display picker remains absent.
//!
//! ## What is otherwise absent
//!
//! Standard `ard` refuses resize because it shares physical displays;
//! `ard-high-performance` can resize its virtual display. `ard-high-performance`
//! takes its picture and sound from the media stream once it is up
//! ([`crate::vnc_apple_media`]): zlib rectangles carry the picture only until
//! then. `ard` keeps its picture on zlib, and its sound arrives over the AirPlay
//! workaround.
//! Every Apple subtype uses Apple's native pasteboard protocol, enabling
//! monitoring before the rekey and carrying fetches and clipboard data inside the
//! encrypted transport.
//!
//! ## Reading the offsets in here
//!
//! The reference document is reverse-engineered and says so; several offsets in it
//! disagree with the bytes a live Mac sends — a layout has a display count the
//! reference lacks — and where they do the comments name which reading is
//! implemented and what settled it: a measurement, or Apple's binaries — see
//! docs/apple-vnc-889.md.

use std::collections::HashMap;

use log::{debug, warn};

use crate::protocol::{
    CursorShape, CursorUnit, DisplayInfo, MAX_CURSOR_DIM, MosaicRect, MosaicRegion,
};
use crate::vnc::ENCODING_ZLIB;
use crate::vnc_encodings::inflate_independent;

/// Raw pixels, the standard RFB encoding, still the fallback here.
const ENCODING_RAW: i32 = 0;
/// Standard RFB `DesktopSize` and `LastRect`, both of which have to be in
/// [`ENCODINGS`] and so are named here rather than reached for across modules.
const ENCODING_DESKTOP_SIZE: i32 = -223;
const ENCODING_LAST_RECT: i32 = -224;
// Standard RFB zlib lives in [`crate::vnc`] as `ENCODING_ZLIB`, imported above: it
// is not Apple's alone, since every generic target is offered it too.
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
/// The pointer's position, in the rectangle header and with no payload. Advertised
/// and ignored: a client draws the pointer where it last put it.
pub const ENCODING_CURSOR_POS: i32 = 0x44c;
/// An older, simpler display list than [`ENCODING_DISPLAY_LAYOUT`], carrying no
/// density. Listing it is what makes the Mac report displays at all; with the layout
/// also listed, the layout is what it sends.
pub const ENCODING_DISPLAY_INFO: i32 = 0x44d;

/// What this client advertises to a Mac.
///
/// `screensharingd` resets its display flags on every `SetEncodings` and sets one for
/// each of `DisplayInfo` and the layout it finds in the list, in any order: with both
/// listed it sends the layout, with only `DisplayInfo` the older list, and with
/// neither no display information at all. Order matters for one thing only, the
/// preferred codec, which is the first of zlib, ZRLE and Apple's own codecs listed.
/// Measured on macOS 26.6 and read from the daemon — see
/// docs/apple-vnc-889.md, "Which encodings make the Mac report its displays". zlib is therefore asked for from the start,
/// in both subtypes: measured at 398 KB for a 3200x1800 frame against 23 MB of raw
/// pixels.
///
/// Every entry is decoded or deliberately stepped over, which is a requirement and
/// not a courtesy: a server takes the list as a promise and will send what it finds
/// here. That is why Apple's own private framebuffer codecs are absent — the
/// reference leaves their payload formats unresolved, so advertising them would
/// ask for rectangles this client could only guess at. Media-stream encoding 1010 is
/// absent from the opening list: High Performance adds it in a second
/// `SetEncodings` once the display it offers the stream for exists
/// ([`crate::vnc_apple_media::encodings_with_media_stream`]).
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
    ENCODING_ZLIB,
];

/// Bytes of one entry in a display configuration's mode table.
const MODE_ENTRY: usize = 0x1c;
/// Bytes of a display descriptor before its mode table.
const DESCRIPTOR_HEAD: usize = 0x9c;
/// Bytes of a display configuration before its first descriptor.
const CONFIG_HEAD: usize = 0x0c;
/// Dots per inch used to derive the descriptor's physical dimensions. This is the
/// value that reproduces the dimensions in the reverse-engineered reference.
const NOMINAL_DPI: f32 = 132.0;
/// Native Screen Sharing's fixed dynamic-display backing ceiling. These are the
/// descriptor's bounds, not its current mode: macOS fixes them when it creates the
/// virtual display, so deriving them from the initial mode silently caps every
/// later resize to that initial width and height.
const DYNAMIC_MAX_WIDTH: u32 = 3840;
const DYNAMIC_MAX_HEIGHT: u32 = 2160;
/// Bytes of one display record in a layout payload.
const LAYOUT_RECORD: usize = 0x38;
/// Bytes of a layout payload before its first record, counted after the `u16`
/// length prefix. The display count is the header's last field.
const LAYOUT_HEAD: usize = 0x14;
/// The most displays Apple's viewer accepts in one layout.
const LAYOUT_MAX_DISPLAYS: usize = 25;
/// Identify this client to Screen Sharing before enabling its optional control
/// messages. The 62-byte body is the native numeric-version form measured on
/// macOS 26; there are no counted strings in it.
pub fn viewer_info() -> [u8; 66] {
    let mut msg = [0u8; 66];
    msg[0] = 0x21;
    msg[2..4].copy_from_slice(&62u16.to_be_bytes());
    msg[4..6].copy_from_slice(&1u16.to_be_bytes()); // generic viewer
    msg[6..10].copy_from_slice(&2u32.to_be_bytes()); // Screen Sharing
    msg[10..14].copy_from_slice(&6u32.to_be_bytes());
    msg[14..18].copy_from_slice(&1u32.to_be_bytes());
    // App patch, then OS major/minor/patch. The daemon handles keys from a viewer
    // it believes older than 10.15 the old way, so this claims the macOS the
    // native values above were measured on, 26.6.2.
    for (at, part) in [(22, 26u32), (26, 6), (30, 2)] {
        msg[at..at + 4].copy_from_slice(&part.to_be_bytes());
    }
    // The final 32 bytes are the command-support bitmap native sends.
    msg[34] = 0xb0;
    msg[36] = 0x0c;
    msg[37] = 0x03;
    msg[38] = 0x90;
    msg[44] = 0x40;
    msg
}

/// Ask for an ordinary controlling session. ServerInit already establishes this,
/// but Apple's native clients send the explicit message before enabling pasteboard
/// monitoring and the order affects which status notifications the Mac emits.
pub fn set_mode_control() -> [u8; 4] {
    [0x0a, 0, 0, 1]
}

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

/// `SetEncryption(command = 2, argument = 1)`: tell the server to decrypt every
/// client message it receives after the rekey.
///
/// The daemon names the resulting state exactly that way. Argument zero disables
/// receive-side decryption; this is not a request to stop encrypting the server's
/// stream.
pub fn enable_inbound_record_decryption() -> Vec<u8> {
    vec![0x12, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00]
}

/// The least time between two updates the Mac pushes unasked once
/// [`auto_framebuffer_update`] has armed it, in microseconds.
///
/// Zero lets it push every frame it captures while the screen changes — a playing
/// video drew 60–90 updates a second. Its sender holds the viewer's lock while it
/// writes each one, and reading this client's next message needs the same lock, so
/// a gateway that drains more slowly than the Mac pushes has its clicks, keys and
/// display changes left unread for as long as the video plays. Pixels come from
/// polling instead; this leaves the push path at one update a second.
const AUTO_UPDATE_INTERVAL_US: u32 = 1_000_000;

/// `AutoFrameBufferUpdate`: arm Apple's optional server-driven updates, paced by
/// `AUTO_UPDATE_INTERVAL_US`.
///
/// Cursor shapes above all depend on this arming across a login, lock or
/// fast-user-switch, so it is re-sent for the full framebuffer whenever the
/// display layout changes.
pub fn auto_framebuffer_update((w, h): (u16, u16)) -> Vec<u8> {
    let mut msg = Vec::with_capacity(16);
    msg.push(0x09);
    msg.push(0);
    msg.extend_from_slice(&1u16.to_be_bytes()); // version
    // The update interval. This is not a display id: `SetDisplayMessage` is the
    // one and only place a screen is selected.
    msg.extend_from_slice(&AUTO_UPDATE_INTERVAL_US.to_be_bytes());
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

/// `SetServerScaling`: ask Standard Screen Sharing to render its framebuffer at
/// `scale` before it is encoded.
///
/// This is not one of Apple's length-prefixed control messages. Native writes a
/// two-byte header followed directly by one big-endian `f64`, and the daemon
/// accepts factors in `(0, 1]`. The next [`Layout`] reports the applied factor in
/// every display record's viewer-scale field.
pub fn set_server_scaling(scale: f32) -> Vec<u8> {
    debug_assert!(scale.is_finite() && scale > 0.0 && scale <= 1.0);
    let mut msg = Vec::with_capacity(10);
    msg.extend_from_slice(&[0x08, 0x00]);
    msg.extend_from_slice(&f64::from(scale).to_be_bytes());
    msg
}

/// One virtual-display mode: the pixels the Mac renders and the points a window
/// occupies. Equal on a 1x screen; a Retina client earns `pixels = 2 × scaled`,
/// which is the shape native Screen Sharing requests from a Retina Mac.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualMode {
    pub pixels: (u16, u16),
    pub scaled: (u16, u16),
}

/// The mode `points` at `density` resolves to, held under the fixed dynamic
/// backing ceiling.
///
/// The ceiling bounds *pixels*, so a Retina density halves the points it can
/// cover. A request past it is shrunk rather than sent as asked: the live Mac
/// answers an out-of-bounds configuration with its old layout, which reads as a
/// resize that silently never took. Shrinking keeps the density — sharp and
/// smaller wins over large and declined — and re-derives the points from the
/// clamped pixels so the two still state the same ratio.
pub fn virtual_display_mode((w, h): (u16, u16), density: f32) -> VirtualMode {
    let axis = |points: u16, max: u32| {
        let pixels = (f32::from(points) * density)
            .round()
            .clamp(1.0, max as f32);
        let scaled = (pixels / density).round().max(1.0);
        (pixels as u16, scaled as u16)
    };
    let (wp, ws) = axis(w, DYNAMIC_MAX_WIDTH);
    let (hp, hs) = axis(h, DYNAMIC_MAX_HEIGHT);
    VirtualMode { pixels: (wp, hp), scaled: (ws, hs) }
}

/// The virtual display's refresh rate, in hertz, which is what bounds the media
/// stream's picture rate ([`crate::vnc_apple_media`]): its 60 fps flag does not.
/// Under a full-screen animation the Mac sent about 57 pictures a second at 60 Hz,
/// with or without the flag, and 30.0 at 30 Hz. The browser is sent 30 frames a
/// second ([`crate::encode`]), so a faster display only doubles the HEVC decoding.
pub const DISPLAY_HZ: u8 = 30;

/// `SetDisplayConfiguration`: request one virtual display whose only advertised
/// mode is `mode`, refreshed [`DISPLAY_HZ`] times a second.
///
/// This is sent while establishing a virtual-display session and again for
/// each accepted viewport change. `display_flags` bit 0 enables dynamic resolution;
/// it is deliberately set even for the initial configured size, so reconnecting
/// restores the Mac's Dynamic resolution checkbox to on if it was changed there.
/// The native one/two-virtual-display control is not implemented.
///
/// The descriptor layout follows the reverse-engineered wire specification: the
/// mode's leading dimensions are the render (pixel) resolution, the scaled pair
/// the logical resolution, and the physical millimetres follow the logical size —
/// a denser screen has more pixels, not more glass.
pub fn set_display_configuration(mode: VirtualMode) -> Vec<u8> {
    let VirtualMode { pixels, scaled } = mode;
    let descriptor = DESCRIPTOR_HEAD + MODE_ENTRY;
    let mut body = Vec::with_capacity(CONFIG_HEAD - 4 + descriptor);
    body.extend_from_slice(&1u16.to_be_bytes()); // version
    body.extend_from_slice(&1u16.to_be_bytes()); // display_count
    body.extend_from_slice(&0u32.to_be_bytes()); // flags

    let mut display = Vec::with_capacity(descriptor);
    display.extend_from_slice(
        &u16::try_from(descriptor)
            .expect("descriptor within u16")
            .to_be_bytes(),
    );
    display.resize(0x7a, 0); // opaque 120-byte region
    display.extend_from_slice(&1u32.to_be_bytes()); // display_flags: dynamic resolution
    display.extend_from_slice(&4u32.to_be_bytes()); // display_type: virtual
    let mm = |px: u16| (f32::from(px) / NOMINAL_DPI * 25.4).to_be_bytes();
    display.extend_from_slice(&mm(scaled.0));
    display.extend_from_slice(&mm(scaled.1));
    display.extend_from_slice(&DYNAMIC_MAX_WIDTH.to_be_bytes());
    display.extend_from_slice(&DYNAMIC_MAX_HEIGHT.to_be_bytes());
    display.extend_from_slice(&0u16.to_be_bytes()); // current_mode_index
    display.extend_from_slice(&0u16.to_be_bytes()); // preferred_mode_index
    // Native Screen Sharing's full dynamic descriptor sends 7 here. The field is
    // passed to SLVirtualDisplaySettings as `rotations`; the exact bit meanings are
    // private, but matching the captured dynamic shape matters more than guessing a
    // tidier upright-only value.
    display.extend_from_slice(&7u32.to_be_bytes());
    display.extend_from_slice(&1u16.to_be_bytes()); // mode_count
    debug_assert_eq!(display.len(), DESCRIPTOR_HEAD);
    for value in [pixels.0, pixels.1, scaled.0, scaled.1] {
        display.extend_from_slice(&u32::from(value).to_be_bytes());
    }
    display.extend_from_slice(&f64::from(DISPLAY_HZ).to_be_bytes()); // refresh_rate_hz
    display.extend_from_slice(&0u32.to_be_bytes()); // mode flags
    debug_assert_eq!(display.len(), descriptor);

    body.extend_from_slice(&display);
    message(SET_DISPLAY_CONFIGURATION, &body)
}

/// [`set_display_configuration`]'s message type.
const SET_DISPLAY_CONFIGURATION: u8 = 0x1d;

/// Whether a Mac can hold a High Performance session: the command bitmap of its
/// enhanced ServerInit lists [`set_display_configuration`], without which there is
/// no virtual display. Apple's viewer asks exactly this before it configures one
/// (`-[SSSession doesServerSupportProMode]`), and a Mac that fails it is connected in
/// Standard mode, after asking the user. See docs/apple-vnc-889.md, "ServerInit's
/// name field is not a name".
pub fn holds_high_performance(commands: &[u8; 16]) -> bool {
    accepts(commands, SET_DISPLAY_CONFIGURATION)
}

/// Whether the command bitmap lists client message `kind`, most significant bit
/// first, as the viewer's `RFBServerCommandSupported` reads it.
fn accepts(commands: &[u8; 16], kind: u8) -> bool {
    commands
        .get(usize::from(kind >> 3))
        .is_some_and(|byte| byte >> (7 - (kind & 7)) & 1 == 1)
}

/// An Apple control message: type, a reserved byte, then the body's length and
/// the body. The length counts the body alone.
fn message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4 + body.len());
    msg.push(kind);
    msg.push(0);
    msg.extend_from_slice(
        &u16::try_from(body.len())
            .expect("body within u16")
            .to_be_bytes(),
    );
    msg.extend_from_slice(body);
    msg
}

/// One screen out of a display layout: what a client is offered, plus the two
/// facts the gateway needs about it that a client never sees.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    pub info: DisplayInfo,
    /// Pixels per point on *this* screen, as the Mac states it: the record's
    /// leading `f64`, or the ratio of its two rects divided by `viewer_scale`
    /// when that is 0.0. 1.0 or 2.0 on every Mac measured.
    pub density: f32,
    /// The server-side framebuffer scale applied to this screen. Apple reports
    /// the same value in every record; keeping it with the record lets the
    /// effective density be checked against that record's two rectangles.
    pub viewer_scale: f32,
    /// This screen's backing-pixel size. The full repaint after a combined
    /// layout consists of one such region per non-mirrored display; gaps in the
    /// bounding framebuffer are not rectangles the Mac sends.
    pub backing: (u16, u16),
    /// Where the backing region sits in the framebuffer, left then top.
    pub backing_at: (i16, i16),
    /// This screen's size in points, and where it sits in the Mac's own
    /// arrangement, left then top.
    pub logical: (u16, u16),
    pub logical_at: (i16, i16),
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
    /// The effective density of the *selected* screen, after Apple's server-side
    /// scaling.
    ///
    /// The combined view is the densest screen's effective density. Over screens
    /// of one density that is every screen's. Over mixed densities no one number
    /// is true, and the browser presents the view through [`Layout::mosaic`]
    /// instead; this remains the Mac's own number, never one made up to fit.
    ///
    /// A combined view of *one* screen is that screen. This is not a corner: a
    /// High Performance layout always reports the combined sentinel over its
    /// single virtual display, so the sentinel path is the one a granted Retina
    /// mode comes back on — reading it as 1x told the client to show 3456x1804
    /// backing pixels at full size, and poisoned the point arithmetic every later
    /// resize starts from.
    ///
    /// [`UNSCALED`]: crate::protocol::UNSCALED
    pub fn scale(&self) -> f32 {
        let Some(id) = self.current else {
            return self
                .displays
                .iter()
                .map(Display::effective_density)
                .fold(crate::protocol::UNSCALED, f32::max);
        };
        self.displays
            .iter()
            .find(|d| d.info.id == id)
            .map_or(crate::protocol::UNSCALED, Display::effective_density)
    }

    /// The server-side factor Apple says it applied to this framebuffer.
    ///
    /// `SetServerScaling` is connection-wide, so every usable record is expected
    /// to agree. The parser warns about a disagreement and this takes the first
    /// record, matching the framebuffer that was actually returned rather than
    /// inventing another value.
    pub fn viewer_scale(&self) -> f32 {
        self.displays[0].viewer_scale
    }

    /// The server scale that makes `selection` match the browser display's
    /// density, without asking Apple to enlarge pixels — the factor Apple's
    /// viewer derives from its own screen's density.
    ///
    /// A selected screen uses its own density, and All Displays over screens of
    /// one density uses theirs: a 2880x1800 backing for a 1440x900 Retina display
    /// arrives at 1440x900 on a 1x browser.
    pub fn server_scale_for(&self, selection: Option<u32>, host_density: f32) -> f32 {
        // A mosaic of mixed densities is composed in the browser from the Mac's
        // own pixels, as Apple's viewer composes it: the daemon logs no
        // `SetServerScaling` from Screen Sharing.app in that view, only the
        // native combined framebuffer at 1.0.
        if selection.is_none() && self.mixed_density() {
            return 1.0;
        }
        let density = selection
            .and_then(|id| self.displays.iter().find(|display| display.info.id == id))
            .map_or_else(
                || self.displays.iter().map(|display| display.density).fold(1.0, f32::max),
                |display| display.density,
            );
        (host_density / density).clamp(f32::MIN_POSITIVE, 1.0)
    }

    /// Whether some screens are at 1x and others are not, which no single
    /// framebuffer scale can then describe — the test Apple's viewer applies.
    /// Screens that are all HiDPI, at whatever densities, are not mixed to Apple
    /// either.
    fn mixed_density(&self) -> bool {
        let unscaled = |display: &Display| (display.density - 1.0).abs() <= 0.005;
        self.displays.iter().any(unscaled) && !self.displays.iter().all(unscaled)
    }

    /// How a client presents the combined view of screens at different
    /// densities, or `None` where one density describes the framebuffer — a
    /// selected screen, or screens that agree.
    ///
    /// Each region keeps the pixels the Mac sent and places them at the screen's
    /// points in its own arrangement. Both spaces are moved to start at zero:
    /// the framebuffer already does, and the arrangement's origin is wherever the
    /// main screen puts it.
    pub fn mosaic(&self) -> Option<Vec<MosaicRegion>> {
        if self.current.is_some() || !self.mixed_density() {
            return None;
        }
        let min = |at: fn(&Display) -> (i16, i16)| {
            self.displays.iter().map(at).fold((i16::MAX, i16::MAX), |m, (x, y)| {
                (m.0.min(x), m.1.min(y))
            })
        };
        // From the least edge, so never negative, and within the u16 the edges
        // were sent in.
        let from = |at: i16, origin: i16| (i32::from(at) - i32::from(origin)) as u16;
        let pixel_origin = min(|display| display.backing_at);
        let point_origin = min(|display| display.logical_at);
        Some(
            self.displays
                .iter()
                .map(|display| MosaicRegion {
                    pixels: MosaicRect {
                        x: from(display.backing_at.0, pixel_origin.0),
                        y: from(display.backing_at.1, pixel_origin.1),
                        w: display.backing.0,
                        h: display.backing.1,
                    },
                    points: MosaicRect {
                        x: from(display.logical_at.0, point_origin.0),
                        y: from(display.logical_at.1, point_origin.1),
                        w: display.logical.0,
                        h: display.logical.1,
                    },
                })
                .collect(),
        )
    }

    /// The points every screen spans together, which is how Apple's viewer
    /// names its combined view ("Both Displays: 2720 × 900"). Unlike the
    /// framebuffer, it does not change with the selection.
    pub fn points_spanned(&self) -> (u16, u16) {
        let span = |start: fn(&Display) -> i16, size: fn(&Display) -> u16| {
            let lo = self.displays.iter().map(|d| i32::from(start(d))).min().unwrap_or(0);
            let hi = self
                .displays
                .iter()
                .map(|d| i32::from(start(d)) + i32::from(size(d)))
                .max()
                .unwrap_or(0);
            u16::try_from(hi - lo).unwrap_or(u16::MAX)
        };
        (
            span(|d| d.logical_at.0, |d| d.logical.0),
            span(|d| d.logical_at.1, |d| d.logical.1),
        )
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

impl Display {
    fn effective_density(&self) -> f32 {
        self.density * self.viewer_scale
    }
}

/// Parse an `AppleDisplayLayout` payload: the bytes after the rectangle's `u16`
/// length, which counts everything after itself.
///
/// The header is a `u16` version, the logical size of the union of screens, the
/// framebuffer's backing size, the current display id (`0xffffffff` for the
/// combined view), a session-state word and a `u16` display count. Records follow
/// at `0x38` bytes each: an `f64` scale, an `f64` viewer scale, the display id, the
/// logical and backing rects as `(top, left, bottom, right)`, a flags word and the
/// display's pixel format. The layout is `ScreensharingAgent`'s
/// `EncodeDisplayInfo2ForDaemon`, read by Apple's viewer the same way — see
/// docs/apple-vnc-889.md, "A layout's length counts what follows it".
pub fn parse_layout(payload: &[u8]) -> anyhow::Result<Layout> {
    parse_layout_kind(payload, false)
}

/// Parse a layout returned for the virtual display requested by
/// [`set_display_configuration`]. The payload does not identify the display as
/// virtual, so that fact comes from the subtype that requested it.
pub fn parse_virtual_display_layout(payload: &[u8]) -> anyhow::Result<Layout> {
    parse_layout_kind(payload, true)
}

fn parse_layout_kind(payload: &[u8], virtual_display: bool) -> anyhow::Result<Layout> {
    anyhow::ensure!(
        payload.len() >= LAYOUT_HEAD,
        "a display layout carried {} bytes, too few for a header",
        payload.len()
    );
    let version = be16(payload, 0);
    let count = usize::from(be16(payload, 0x12));
    // Apple's viewer takes 1–25 displays and tolerates bytes past the last record.
    anyhow::ensure!(
        (1..=LAYOUT_MAX_DISPLAYS).contains(&count),
        "a display layout lists {count} displays"
    );
    anyhow::ensure!(
        payload.len() >= LAYOUT_HEAD + count * LAYOUT_RECORD,
        "a display layout lists {count} displays in {} bytes",
        payload.len()
    );
    // `0xffffffff` means the combined view of every screen. Any other value is a
    // screen id, and it is how a selection is confirmed — the gateway believes it
    // acted only when this comes back changed.
    let current = match be32(payload, 0x0a) {
        u32::MAX => None,
        id => Some(id),
    };
    debug!(
        "vnc: display layout version {version}, {count} display(s), current {current:?}, \
         session state {:#x}",
        be32(payload, 0x0e)
    );

    let mut displays = Vec::new();
    let mut origins = Vec::new();
    let (records, _) = payload[LAYOUT_HEAD..].as_chunks::<LAYOUT_RECORD>();
    for (index, record) in records[..count].iter().enumerate() {
        let flags = be32(record, 0x24);
        // Bit 1 is `CGDisplayIsInMirrorSet`, which is true of every member of a
        // mirror set, the one the others copy included. Members share an origin, so
        // the first of them is offered and the rest, the same picture under another
        // name, are not.
        let origin = (be16(record, 0x14), be16(record, 0x16));
        if flags & 0x02 != 0 && origins.contains(&origin) {
            continue;
        }
        // Signed: a screen left of or above the main one has negative edges in
        // the Mac's global arrangement. Every value measured so far is positive,
        // and reads the same either way.
        let signed = |at: usize| be16(record, at).cast_signed();
        let edges = |at: usize| {
            let (top, left, bottom, right) =
                (signed(at), signed(at + 2), signed(at + 4), signed(at + 6));
            let size = |from: i16, to: i16| {
                u16::try_from(i32::from(to) - i32::from(from)).unwrap_or(0)
            };
            (size(left, right), size(top, bottom))
        };
        let logical = edges(0x14);
        let backing = edges(0x1c);
        // (top, left, ...) on the wire, so the left edge is the second field.
        let corner = |at: usize| (signed(at + 2), signed(at));
        // An unusable screen is dropped, the way a mirror copy is, rather than
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
        // The Mac states the native density as the first double and its applied
        // server scale as the second. Their product is the ratio of the returned
        // backing and logical rects, so all three fields check one another.
        let stated = f64::from_be_bytes(
            record[0x00..0x08].try_into().expect("eight bytes inside a 0x38-byte record"),
        );
        let viewer_scale = f64::from_be_bytes(
            record[0x08..0x10].try_into().expect("eight bytes inside a 0x38-byte record"),
        );
        let ratio = f32::from(backing.0) / f32::from(logical.0);
        // Only `SetServerScaling` moves this off 1.0, and a record is not dropped
        // over it: High Performance lists one record, so dropping it would end the
        // session. A value outside (0, 1] is read as the unscaled 1.0 it was
        // before this field was read at.
        let viewer_scale = if viewer_scale.is_finite() && viewer_scale > 0.0 && viewer_scale <= 1.0 {
            viewer_scale as f32
        } else {
            warn!(
                "vnc: display layout record {index} states a viewer scale of {viewer_scale}, \
                 outside (0, 1]; reading it as 1"
            );
            1.0
        };
        // The agent writes 0.0 when it cannot look the screen's mode up ("bad mode
        // ref") and takes the scaled backing rect from the pixel bounds regardless,
        // so undo the viewer scale to recover the native density from the rects.
        let stated = if stated == 0.0 {
            f64::from(ratio / viewer_scale)
        } else {
            stated
        };
        if !stated.is_finite() || !(1.0..=4.0).contains(&stated) {
            warn!(
                "vnc: display layout record {index} states a scale factor of {stated}, \
                 outside 1..=4; not offering it"
            );
            continue;
        }
        let density = stated as f32;
        let effective_density = density * viewer_scale;
        if (ratio - effective_density).abs() > 0.01 {
            warn!(
                "vnc: display {} states density {density} at server scale {viewer_scale} but its \
                 rects give {ratio}; using the stated values",
                be32(record, 0x10)
            );
        }
        // "1600×900 at 2x" — the points a window occupies, which is the size a
        // person recognises, and then the density that earns it more pixels.
        // `f32`'s own Display gives "2" for 2.0 and "1.5" for 1.5, which is exactly
        // the two shapes wanted and neither of them "2.0x". A physical screen at 1x
        // stays bare — the suffix flags the Retina screens in a list of them — but a
        // virtual display's density is a negotiated outcome, so it is always stated:
        // "at 1x" on a display that should be Retina is the whole finding.
        let suffix = if density > 1.005 || virtual_display {
            format!(" at {density}x")
        } else {
            String::new()
        };
        origins.push(origin);
        displays.push(Display {
            info: DisplayInfo {
                id: be32(record, 0x10),
                label: if virtual_display {
                    "Virtual display".to_owned()
                } else {
                    format!("Display {}", index + 1)
                },
                detail: format!("{}×{}{suffix}", logical.0, logical.1),
                main: flags & 0x01 != 0,
                virtual_display,
            },
            density,
            viewer_scale,
            backing,
            backing_at: corner(0x1c),
            logical,
            logical_at: corner(0x14),
        });
    }

    anyhow::ensure!(!displays.is_empty(), "a display layout listed no usable display");
    let viewer_scale = displays[0].viewer_scale;
    if displays
        .iter()
        .skip(1)
        .any(|display| (display.viewer_scale - viewer_scale).abs() > 0.005)
    {
        warn!("vnc: display layout records disagree about the server scaling factor");
    }
    let backing = (be16(payload, 0x06), be16(payload, 0x08));
    anyhow::ensure!(
        backing.0 > 0 && backing.1 > 0,
        "a display layout gives a {}x{} framebuffer",
        backing.0,
        backing.1
    );
    Ok(Layout { backing, current, displays })
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
    /// never says how it is spelled.
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
        // Each store is an independent zlib stream. Unlike framebuffer encoding 6,
        // Apple does not carry the deflate window from one cursor image to the next.
        // A fresh inflater also means a malformed shape can be skipped without
        // poisoning every cursor that follows it.
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
        let raw = inflate_independent("cursor image", deflated, pixels * 4 + pixels)?;
        // BGRA pixels, then a *separate* alpha plane. The fourth byte of each
        // pixel is not the alpha — folding it in is how a cursor comes out
        // uniformly opaque or invisible.
        let (bgrx, alpha) = raw.split_at(pixels * 4);
        let mut rgba = Vec::with_capacity(pixels * 4);
        for (px, &a) in bgrx.as_chunks::<4>().0.iter().zip(alpha) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], a]);
        }
        // Point-sized: the Mac ships the same pixmap whatever density the display
        // renders at, and re-selects it from this cache across density changes.
        let shape = CursorShape::from_rgba(w, h, hx, hy, CursorUnit::Points, &rgba)?;
        debug!("vnc: cursor {w}x{h} stored as {id}, {} bytes", shape.png.len());
        self.shapes.insert(id, shape.clone());
        Ok(Cursor::Shape(shape))
    }
}

/// One display as a test writes it: id, logical size, backing size, flags.
#[cfg(test)]
pub(crate) type TestScreen = (u32, (u16, u16), (u16, u16), u32);

/// Build a layout payload, for the cases the captured bytes in this module's tests
/// cannot cover — a mirrored screen, a selection, a malformed count.
///
/// Lives outside `mod tests` because [`crate::vnc`]'s tests need it too, and two
/// copies of this bit-twiddling would have to be kept in step with the parser by
/// hand. Cross-checked against the capture by
/// `the_builder_agrees_with_the_captured_bytes`, so a drift between builder and
/// parser fails a test rather than agreeing with itself.
#[cfg(test)]
pub(crate) fn test_layout(current: Option<u32>, displays: &[TestScreen]) -> Vec<u8> {
    let mut payload = vec![0u8; LAYOUT_HEAD];
    payload[0..2].copy_from_slice(&5u16.to_be_bytes());
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
    payload[2..4].copy_from_slice(&span.to_be_bytes());
    payload[4..6].copy_from_slice(&first.1.1.to_be_bytes());
    payload[6..8].copy_from_slice(&framebuffer.0.to_be_bytes());
    payload[8..10].copy_from_slice(&framebuffer.1.to_be_bytes());
    payload[10..14].copy_from_slice(&current.unwrap_or(u32::MAX).to_be_bytes());
    payload[14..18].copy_from_slice(&4u32.to_be_bytes()); // on console
    payload[18..20].copy_from_slice(&u16::try_from(displays.len()).unwrap().to_be_bytes());
    let mut left = 0u16;
    for (id, logical, backing, flags) in displays {
        let mut r = vec![0u8; LAYOUT_RECORD];
        let density = f64::from(backing.0) / f64::from(logical.0.max(1));
        r[0x00..0x08].copy_from_slice(&density.to_be_bytes());
        r[0x08..0x10].copy_from_slice(&1.0f64.to_be_bytes());
        r[0x10..0x14].copy_from_slice(&id.to_be_bytes());
        // (top, left, bottom, right), laid out left to right.
        let edges = |at: usize, r: &mut [u8], w: u16, h: u16, x: u16| {
            r[at..at + 2].copy_from_slice(&0u16.to_be_bytes());
            r[at + 2..at + 4].copy_from_slice(&x.to_be_bytes());
            r[at + 4..at + 6].copy_from_slice(&h.to_be_bytes());
            r[at + 6..at + 8].copy_from_slice(&(x + w).to_be_bytes());
        };
        edges(0x14, &mut r, logical.0, logical.1, left);
        edges(0x1c, &mut r, backing.0, backing.1, left);
        r[0x24..0x28].copy_from_slice(&flags.to_be_bytes());
        payload.extend_from_slice(&r);
        left += logical.0;
    }
    payload
}

/// Restate a [`test_layout`] payload as the Mac sends it after
/// `SetServerScaling`: every record's native density from `natives`, in order,
/// and `viewer` as its applied factor. The builder derives the density from the
/// rects, which after scaling no longer give the native one.
#[cfg(test)]
pub(crate) fn test_scale_layout(payload: &mut [u8], viewer: f64, natives: &[f64]) {
    for (index, native) in natives.iter().enumerate() {
        let at = LAYOUT_HEAD + index * LAYOUT_RECORD;
        payload[at..at + 8].copy_from_slice(&native.to_be_bytes());
        payload[at + 8..at + 16].copy_from_slice(&viewer.to_be_bytes());
    }
}

/// [`test_layout`] as it arrives in the rectangle, behind its `u16` length.
#[cfg(test)]
pub(crate) fn test_layout_wire(current: Option<u32>, displays: &[TestScreen]) -> Vec<u8> {
    let payload = test_layout(current, displays);
    let mut wire = u16::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
    wire.extend_from_slice(&payload);
    wire
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
    fn standard_control_prelude_matches_the_apple_wire() {
        let info = viewer_info();
        assert_eq!(&info[..10], &[0x21, 0, 0, 62, 0, 1, 0, 0, 0, 2]);
        assert_eq!(&info[10..22], &[0, 0, 0, 6, 0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(&info[22..34], &[0, 0, 0, 26, 0, 0, 0, 6, 0, 0, 0, 2]);
        let mut bitmap = [0u8; 32];
        bitmap[0] = 0xb0;
        bitmap[2] = 0x0c;
        bitmap[3] = 0x03;
        bitmap[4] = 0x90;
        bitmap[10] = 0x40;
        assert_eq!(info[34..], bitmap);
        assert_eq!(set_mode_control(), [0x0a, 0, 0, 1]);
    }

    /// macvm's bitmap (macOS 26.6.2), which lists `SetDisplayConfiguration`, and the
    /// same with only that bit cleared.
    #[test]
    fn high_performance_needs_the_mac_to_accept_set_display_configuration() {
        let macvm = [0xbf, 0xf6, 0xe7, 0x2f, 0xec, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(holds_high_performance(&macvm));
        assert!(accepts(&macvm, 0x1c), "the media stream's configuration");
        assert!(!accepts(&macvm, 0x01), "a message type no Mac takes");

        let mut without = macvm;
        without[3] &= !0x04;
        assert!(!holds_high_performance(&without));
        assert!(!holds_high_performance(&[0; 16]));
        assert!(!accepts(&[0xff; 16], 0x80), "past the bitmap");
    }

    #[test]
    fn set_encryption_is_the_observed_bytes() {
        assert_eq!(
            set_encryption_start(),
            vec![0x12, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01]
        );
        assert_eq!(
            enable_inbound_record_decryption(),
            vec![0x12, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn a_virtual_display_configuration_has_one_mode_under_the_fixed_dynamic_ceiling() {
        let msg = set_display_configuration(virtual_display_mode((1600, 1000), 1.0));
        assert_eq!(msg[0], 0x1d);
        assert_eq!(usize::from(be16(&msg, 2)), msg.len() - 4);
        assert_eq!(msg.len(), CONFIG_HEAD + DESCRIPTOR_HEAD + MODE_ENTRY);
        assert_eq!(be16(&msg, 4), 1); // version
        assert_eq!(be16(&msg, 6), 1); // display_count

        let display = &msg[CONFIG_HEAD..];
        assert_eq!(usize::from(be16(display, 0)), DESCRIPTOR_HEAD + MODE_ENTRY);
        assert_eq!(be32(display, 0x7a), 1, "display_flags");
        assert_eq!(be32(display, 0x7e), 4, "virtual display_type");
        assert_eq!(be32(display, 0x8a), DYNAMIC_MAX_WIDTH, "maximum width");
        assert_eq!(be32(display, 0x8e), DYNAMIC_MAX_HEIGHT, "maximum height");
        assert_eq!(be16(display, 0x92), 0, "current mode");
        assert_eq!(be16(display, 0x94), 0, "preferred mode");
        assert_eq!(be32(display, 0x96), 7, "native dynamic rotations value");
        assert_eq!(be16(display, 0x9a), 1, "one mode");
        assert_eq!(be32(display, 0x9c), 1600);
        assert_eq!(be32(display, 0xa0), 1000);
        assert_eq!(be32(display, 0xa4), 1600, "1x scaled width");
        assert_eq!(be32(display, 0xa8), 1000, "1x scaled height");
        assert_eq!(&display[0xac..0xb4], &[0x40, 0x3e, 0, 0, 0, 0, 0, 0], "30 Hz");
        assert_eq!(be32(display, 0xb4), 0, "mode flags");

        // The ceiling is a capability of the virtual display, not another copy of
        // its mode. If the initial 1280x800 request put 1280x800 here, the live Mac
        // would reject an otherwise valid 1281x600 steady-state configuration and
        // answer with the old layout.
        let smaller = set_display_configuration(virtual_display_mode((1280, 800), 1.0));
        let display = &smaller[CONFIG_HEAD..];
        assert_eq!(be32(display, 0x8a), DYNAMIC_MAX_WIDTH);
        assert_eq!(be32(display, 0x8e), DYNAMIC_MAX_HEIGHT);
    }

    #[test]
    fn a_retina_mode_doubles_the_pixels_and_keeps_the_points() {
        let mode = virtual_display_mode((1600, 1000), 2.0);
        assert_eq!(mode, VirtualMode { pixels: (3200, 2000), scaled: (1600, 1000) });

        let msg = set_display_configuration(mode);
        let display = &msg[CONFIG_HEAD..];
        assert_eq!(be32(display, 0x9c), 3200, "render width");
        assert_eq!(be32(display, 0xa0), 2000, "render height");
        assert_eq!(be32(display, 0xa4), 1600, "scaled width");
        assert_eq!(be32(display, 0xa8), 1000, "scaled height");
        // The glass does not grow with the density: physical millimetres follow
        // the logical size, so 1x and 2x modes of the same points agree here.
        let one_x = set_display_configuration(virtual_display_mode((1600, 1000), 1.0));
        assert_eq!(display[0x82..0x8a], one_x[CONFIG_HEAD..][0x82..0x8a]);
    }

    #[test]
    fn the_backing_ceiling_shrinks_an_oversized_retina_mode_instead_of_sending_it() {
        // 2560×1440 points at 2x would be 5120×2880 pixels — past the fixed
        // 3840×2160 backing ceiling the Mac holds a virtual display to. The pixels
        // are clamped and the points re-derived, keeping the density exact.
        let mode = virtual_display_mode((2560, 1440), 2.0);
        assert_eq!(mode, VirtualMode { pixels: (3840, 2160), scaled: (1920, 1080) });

        // At 1x the same points fit untouched.
        assert_eq!(
            virtual_display_mode((2560, 1440), 1.0),
            VirtualMode { pixels: (2560, 1440), scaled: (2560, 1440) }
        );
    }

    #[test]
    fn arming_display_selection_and_server_scaling_are_fixed_shapes() {
        let arm = auto_framebuffer_update((3840, 2160));
        assert_eq!(arm.len(), 16);
        assert_eq!(arm[0], 0x09);
        assert_eq!(be16(&arm, 2), 1);
        assert_eq!(be32(&arm, 4), 1_000_000, "one unasked update a second at most");
        assert_eq!(be16(&arm, 12), 3840);
        assert_eq!(be16(&arm, 14), 2160);

        let pick = set_display_message(Some(0x2b00_4501));
        assert_eq!(pick, vec![0x0d, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x45, 0x01]);
        assert_eq!(pick[1], 0, "one named screen");

        let all = set_display_message(None);
        assert_eq!(all, vec![0x0d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

        assert_eq!(
            set_server_scaling(0.5),
            vec![0x08, 0x00, 0x3f, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    /// Dropping `DisplayInfo` or the layout from the list costs the display
    /// information silently, with a session that still connects and paints.
    #[test]
    fn the_list_asks_for_displays_and_zlib_but_not_the_media_stream() {
        for encoding in [ENCODING_DISPLAY_INFO, ENCODING_DISPLAY_LAYOUT, ENCODING_ZLIB] {
            assert!(ENCODINGS.contains(&encoding), "{encoding:#x}");
        }
        assert!(!ENCODINGS.contains(&0x3f2), "the media stream is asked for once a display exists");
    }

    /// The `AppleDisplayLayout` a macOS 26 VM sent for its two real screens, off
    /// the wire after the rectangle's `u16` length of 132 (see
    /// docs/apple-vnc-889.md). The capture was taken by a reader that stopped four
    /// bytes short; those four — the last record's blue shift and three pad bytes —
    /// are zero in every layout `ScreensharingAgent` builds, and are restored here.
    ///
    /// Captured rather than constructed, because a payload this parser built for
    /// itself would agree with whichever offsets it happened to use. The ground
    /// truth these bytes have to reproduce was measured separately, over SSH: display ids 1 and
    /// 4, a 1280x800 at (0,0) and a 1600x900 at (1280,0), the first one main, and
    /// the second one Retina.
    const TWO_REAL_SCREENS: &[u8] = &[
        // header: version 5, logical 2880x900, backing 4480x1800, current display
        // 0xffffffff (the combined view), session state 4 (on console), 2 displays.
        0x00, 0x05, 0x0b, 0x40, 0x03, 0x84, 0x11, 0x80, 0x07, 0x08, 0xff, 0xff, 0xff, 0xff, 0x00,
        0x00, 0x00, 0x04, 0x00, 0x02, //
        // display id 1: scale 1.0, viewer scale 1.0, both rects (0,0,800,1280), main.
        0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0xf0, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x03, 0x20, 0x05, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x03, 0x20, 0x05, 0x00, 0x00, 0x00, 0x00, 0x01, 0x20, 0x20, 0x00,
        0x01, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff, 0x10, 0x08, 0x00, 0x00, 0x00, 0x00, //
        // display id 4: scale 2.0, logical (0,1280,900,2880), backing (0,1280,1800,4480).
        0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0xf0, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x05, 0x00, 0x03, 0x84, 0x0b, 0x40,
        0x00, 0x00, 0x05, 0x00, 0x07, 0x08, 0x11, 0x80, 0x00, 0x00, 0x00, 0x00, 0x20, 0x20, 0x00,
        0x01, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff, 0x10, 0x08, 0x00, 0x00, 0x00, 0x00,
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
        // Combined: the densest screen's, which server scaling matches to the
        // browser — never the ratio of the header's own two geometries
        // (4480/2880), which describes neither screen.
        let combined = parse_layout(TWO_REAL_SCREENS).unwrap();
        assert_eq!(combined.scale(), 2.0);

        // Selecting one screen makes it exact, which is the whole reason picking
        // matters. Both edits are what the Mac actually answered a `0x0d` with: the
        // framebuffer narrows to that screen and its id lands in `current_display`.
        let mut payload = TWO_REAL_SCREENS.to_vec();
        payload[0x06..0x08].copy_from_slice(&3200u16.to_be_bytes());
        payload[0x0a..0x0e].copy_from_slice(&4u32.to_be_bytes());
        let retina = parse_layout(&payload).unwrap();
        assert_eq!(retina.current, Some(4));
        assert_eq!(retina.backing, (3200, 1800));
        assert_eq!(retina.scale(), 2.0);
        assert_eq!(retina.repaint_pixels(), 3200 * 1800);

        payload[0x06..0x08].copy_from_slice(&1280u16.to_be_bytes());
        payload[0x08..0x0a].copy_from_slice(&800u16.to_be_bytes());
        payload[0x0a..0x0e].copy_from_slice(&1u32.to_be_bytes());
        let plain = parse_layout(&payload).unwrap();
        assert_eq!(plain.scale(), 1.0);
        assert_eq!(plain.backing, (1280, 800));

        // A screen that is gone by the time the id is looked up leaves the desktop
        // at its pixel size rather than guessing at another screen's density.
        payload[0x0a..0x0e].copy_from_slice(&99u32.to_be_bytes());
        assert_eq!(parse_layout(&payload).unwrap().scale(), crate::protocol::UNSCALED);
    }

    #[test]
    fn server_scaling_matches_the_chosen_screen_to_the_browser_density() {
        let combined = parse_layout(TWO_REAL_SCREENS).unwrap();
        assert_eq!(combined.viewer_scale(), 1.0);
        // All Displays over mixed densities is composed from the native pixels.
        assert_eq!(combined.server_scale_for(None, 1.0), 1.0);
        assert_eq!(combined.server_scale_for(None, 2.0), 1.0);
        assert_eq!(combined.server_scale_for(Some(1), 1.0), 1.0);
        assert_eq!(combined.server_scale_for(Some(4), 1.0), 0.5);

        // Apple keeps the native density in the first double and reports the
        // applied server scale in the second. The returned backing rect contains
        // the already-scaled pixels; their effective density is what the browser
        // is told, so it maps one returned pixel to one device pixel at 1x.
        let mut payload = layout(Some(4), &[(4, (1440, 900), (1440, 900), 0x01)]);
        super::test_scale_layout(&mut payload, 0.5, &[2.0]);
        let scaled = parse_layout(&payload).unwrap();
        assert_eq!(scaled.displays[0].density, 2.0);
        assert_eq!(scaled.viewer_scale(), 0.5);
        assert_eq!(scaled.scale(), 1.0);

        // All Displays at 0.5: the Retina screen arrives at 1x and the 1x screen
        // at 0.5x. The densest is what the browser was matched to.
        let mut payload = layout(
            None,
            &[(1, (1280, 800), (640, 400), 0x01), (4, (1600, 900), (1600, 900), 0x00)],
        );
        super::test_scale_layout(&mut payload, 0.5, &[1.0, 2.0]);
        let both = parse_layout(&payload).unwrap();
        assert_eq!(both.displays.len(), 2);
        assert_eq!(both.viewer_scale(), 0.5);
        assert_eq!(both.scale(), 1.0);
    }

    #[test]
    fn a_screen_left_of_the_main_one_keeps_its_negative_place() {
        // A 2x 1440x900 screen at x = -1440 points beside the 1x main screen at
        // 0. Read unsigned, its left edge wrapped past the right one and the
        // screen was dropped as zero-width.
        let mut payload = layout(
            None,
            &[(7, (1440, 900), (2880, 1800), 0x00), (1, (1280, 800), (1280, 800), 0x01)],
        );
        let record = LAYOUT_HEAD;
        payload[record + 0x16..record + 0x18].copy_from_slice(&(-1440i16).to_be_bytes());
        payload[record + 0x1a..record + 0x1c].copy_from_slice(&0i16.to_be_bytes());
        let second = LAYOUT_HEAD + LAYOUT_RECORD;
        payload[second + 0x16..second + 0x18].copy_from_slice(&0i16.to_be_bytes());
        payload[second + 0x1a..second + 0x1c].copy_from_slice(&1280i16.to_be_bytes());
        let parsed = parse_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 2);
        assert_eq!(parsed.displays[0].logical, (1440, 900));
        assert_eq!(parsed.points_spanned(), (2720, 900));
        let mosaic = parsed.mosaic().unwrap();
        assert_eq!((mosaic[0].points.x, mosaic[1].points.x), (0, 1440));
    }

    #[test]
    fn an_unusable_viewer_scale_reads_as_unscaled_rather_than_dropping_the_screen() {
        let mut payload = layout(None, &[(9, (1728, 902), (3456, 1804), 0x01)]);
        super::test_scale_layout(&mut payload, 0.0, &[2.0]);
        let parsed = parse_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 1);
        assert_eq!(parsed.viewer_scale(), 1.0);
        assert_eq!(parsed.scale(), 2.0);
    }

    #[test]
    fn only_a_mixed_density_combined_view_is_composed() {
        // The captured Mac: a 1x screen at (0,0) beside a 2x one at 1280 points,
        // whose pixels start 1280 pixels in.
        let combined = parse_layout(TWO_REAL_SCREENS).unwrap();
        let rect = |x, y, w, h| MosaicRect { x, y, w, h };
        assert_eq!(
            combined.mosaic(),
            Some(vec![
                MosaicRegion { pixels: rect(0, 0, 1280, 800), points: rect(0, 0, 1280, 800) },
                MosaicRegion {
                    pixels: rect(1280, 0, 3200, 1800),
                    points: rect(1280, 0, 1600, 900),
                },
            ])
        );

        assert_eq!(combined.points_spanned(), (2880, 900), "what Apple's viewer names it");

        // A selected screen is one density, and so are screens that agree.
        let mut payload = TWO_REAL_SCREENS.to_vec();
        payload[0x0a..0x0e].copy_from_slice(&4u32.to_be_bytes());
        assert_eq!(parse_layout(&payload).unwrap().mosaic(), None);
        let alike = layout(None, &[(1, (1280, 800), (2560, 1600), 0x01), (2, (1440, 900), (2880, 1800), 0x00)]);
        let alike = parse_layout(&alike).unwrap();
        assert_eq!(alike.mosaic(), None);
        assert_eq!(alike.server_scale_for(None, 1.0), 0.5);

        // Mixed is what Apple's viewer calls mixed: a 1x screen beside one that
        // is not. HiDPI screens that differ among themselves are served uniformly.
        let hidpi = layout(None, &[(1, (1280, 800), (1920, 1200), 0x01), (2, (1440, 900), (2880, 1800), 0x00)]);
        let hidpi = parse_layout(&hidpi).unwrap();
        assert_eq!(hidpi.mosaic(), None);
        assert_eq!(hidpi.server_scale_for(None, 1.0), 0.5);
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
        // Not byte-equal: the capture carries a pixel format per record that nothing
        // here reads. Equal in every field the parser does read, which is the claim
        // that matters.
        assert_eq!(parse_layout(&built).unwrap(), parse_layout(TWO_REAL_SCREENS).unwrap());
    }

    #[test]
    fn a_high_performance_layout_is_reported_as_virtual() {
        // The measured Mac reports a virtual display under the combined
        // (`0xffffffff`) sentinel, not by its id.
        let payload = layout(None, &[(9, (1600, 1000), (1600, 1000), 0x01)]);
        let parsed = parse_virtual_display_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 1);
        assert_eq!(parsed.displays[0].info.label, "Virtual display");
        assert!(parsed.displays[0].info.virtual_display);
        // A virtual display states its density even at 1x: whether the negotiated
        // density took is exactly what a person opening the list wants to read.
        assert_eq!(parsed.displays[0].info.detail, "1600×1000 at 1x");
        assert_eq!(parsed.scale(), 1.0);

        // A granted Retina mode comes back on the same sentinel, and the one
        // display's density is the framebuffer's. Reading it as the mixed-mosaic
        // 1x here is what stranded the desktop at 2x: the client showed backing
        // pixels at full size, and the next density change computed its points
        // from the wrong scale and skipped itself as a no-op.
        let retina = layout(None, &[(9, (1600, 1000), (3200, 2000), 0x01)]);
        let parsed = parse_virtual_display_layout(&retina).unwrap();
        assert_eq!(parsed.displays[0].info.detail, "1600×1000 at 2x");
        assert_eq!(parsed.scale(), 2.0);
    }

    #[test]
    fn a_mirrored_screen_is_not_offered_twice() {
        // Every member of a mirror set carries bit 1, the one the others copy
        // included, and they share an origin.
        let mut payload =
            layout(None, &[(11, (1920, 1080), (1920, 1080), 0x03), (22, (1920, 1080), (1920, 1080), 0x02)]);
        let second = LAYOUT_HEAD + LAYOUT_RECORD;
        payload.copy_within(LAYOUT_HEAD + 0x14..LAYOUT_HEAD + 0x24, second + 0x14);
        let parsed = parse_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 1);
        assert_eq!(parsed.displays[0].info.id, 11);
        // One entry is what makes the client hide the picker, which is right:
        // there is nothing to choose.

        // A mirror set's only listed member, which is what hardware mirroring
        // reports, is a screen like any other.
        let alone = parse_layout(&layout(None, &[(11, (1920, 1080), (1920, 1080), 0x03)])).unwrap();
        assert_eq!(alone.displays.len(), 1);
    }

    #[test]
    fn a_layout_that_does_not_add_up_is_refused() {
        let one = |flags| layout(None, &[(11, (800, 600), (800, 600), flags)]);

        // Short of a header.
        assert!(parse_layout(&[0, 5, 0, 0]).is_err());

        // No displays, and more displays than Apple's viewer takes.
        for count in [0u16, 26] {
            let mut payload = one(0x01);
            payload[0x12..0x14].copy_from_slice(&count.to_be_bytes());
            let err = parse_layout(&payload).unwrap_err();
            assert!(format!("{err:#}").contains("displays"), "{err:#}");
        }

        // A count the bytes do not hold.
        let mut payload = one(0x01);
        payload[0x12..0x14].copy_from_slice(&2u16.to_be_bytes());
        let err = parse_layout(&payload).unwrap_err();
        assert!(format!("{err:#}").contains("in 76 bytes"), "{err:#}");

        // Bytes past the last record are tolerated, as Apple's viewer tolerates them.
        let mut payload = one(0x01);
        payload.extend_from_slice(&[0u8; 8]);
        assert_eq!(parse_layout(&payload).unwrap(), parse_layout(&one(0x01)).unwrap());

        // A scale factor read out of the wrong offset: the bytes there are a
        // denormal, not a density. The record is dropped rather than the layout
        // refused, so with only one screen in it what is left is nothing to render —
        // and a *wrong set of offsets* fails exactly here, because it would drop
        // every record.
        let mut payload = one(0x01);
        payload[LAYOUT_HEAD..LAYOUT_HEAD + 0x08].copy_from_slice(&[0, 0, 0x3f, 0xf0, 0, 0, 0, 0]);
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
        payload[LAYOUT_HEAD..LAYOUT_HEAD + 0x08].copy_from_slice(&99.0f64.to_be_bytes());
        let parsed = parse_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 2, "the other two are still offered");
        assert_eq!(parsed.displays[0].info.id, 22);
        assert_eq!(parsed.displays[1].info.id, 33);

        // No scale at all, which the agent writes when it cannot look the mode up:
        // the rects still say 2x.
        let mut payload = layout(None, &screens);
        let second = LAYOUT_HEAD + LAYOUT_RECORD;
        payload[second..second + 0x08].copy_from_slice(&0.0f64.to_be_bytes());
        let parsed = parse_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 3);
        assert_eq!(parsed.displays[1].density, 2.0);

        // A screen of no size, which would otherwise be offered as "0×0".
        let mut payload = layout(None, &screens);
        let second = LAYOUT_HEAD + LAYOUT_RECORD;
        payload[second + 0x14..second + 0x1c].copy_from_slice(&[0u8; 8]);
        let parsed = parse_layout(&payload).unwrap();
        assert_eq!(parsed.displays.len(), 2);
        assert!(parsed.displays.iter().all(|d| d.info.id != 22));
    }

    /// Store once, then select by id — which is nearly every cursor change in a
    /// live session, and the reason the pixels have to be kept.
    #[test]
    fn a_cursor_is_stored_once_and_selected_by_id() {
        use flate2::{Compress, Compression, FlushCompress};

        fn compress(raw: &[u8]) -> Vec<u8> {
            let mut deflate = Compress::new(Compression::default(), true);
            let mut compressed = Vec::with_capacity(raw.len() + 128);
            deflate
                .compress_vec(raw, &mut compressed, FlushCompress::Sync)
                .unwrap();
            compressed
        }

        let (w, h) = (2u16, 2u16);
        let pixels = usize::from(w) * usize::from(h);
        let mut raw = Vec::new();
        for i in 0..pixels {
            // BGRA, whose fourth byte is deliberately *not* the alpha.
            raw.extend_from_slice(&[10 + i as u8, 20, 30, 0xff]);
        }
        raw.extend_from_slice(&[0x00, 0x40, 0x80, 0xff]); // the real alpha plane

        let deflated = compress(&raw);

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

        // A second store begins a new zlib stream. Reusing the first store's
        // inflater here rejects the second stream's zlib header as an invalid
        // stored block, which is the live-session failure this guards against.
        raw[0] = 99;
        let second = cache.accept(1002, (0, 0), (w, h), &compress(&raw)).unwrap();
        assert!(matches!(second, Cursor::Shape(_)));

        // A malformed independent store does not alter the cache or poison the
        // next independently compressed shape.
        assert!(cache.accept(1003, (0, 0), (w, h), &[1, 2, 3]).is_err());
        raw[0] = 101;
        let after_bad = cache.accept(1004, (0, 0), (w, h), &compress(&raw)).unwrap();
        assert!(matches!(after_bad, Cursor::Shape(_)));

        let err = cache.accept(1002, (0, 0), (0, 0), &deflated).unwrap_err();
        assert!(format!("{err:#}").contains("for a 0x0 shape"), "{err:#}");
        let err = cache
            .accept(1003, (0, 0), (MAX_CURSOR_DIM + 1, 32), &deflated)
            .unwrap_err();
        assert!(format!("{err:#}").contains("past the"), "{err:#}");
    }
}
