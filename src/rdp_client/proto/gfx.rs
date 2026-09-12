//! The graphics pipeline's PDUs: what a server says on the Graphics channel, and
//! the two things a client says back.
//!
//! MS-RDPEGFX moves the desktop off the share's bitmap updates and onto a dynamic
//! channel of its own. The server creates *surfaces*, maps them onto the output at
//! an origin, draws into them with codecs and copies, and brackets each batch of
//! drawing in a StartFrame and an EndFrame; the client acknowledges every EndFrame,
//! or the server stops sending. A monitor layout is answered by a ResetGraphics that
//! names the new output size, with no reactivation.
//!
//! Every PDU on the channel wears an eight-byte header — command, flags, and the
//! length including the header — and arrives inside the bulk compression of
//! [`super::zgfx`], which may hold several PDUs at once. So this module reads a
//! *buffer* of PDUs ([`messages`]) rather than one, and decodes each into a
//! [`Message`] the compositor above can act on.
//!
//! # What is decoded and what is only named
//!
//! Every command a server sends is decoded to its fields, whether or not anything
//! above acts on it yet — the shapes are cheap to read and knowing what arrived is
//! how the next decoder gets chosen. A command this client has no reading for at all
//! is [`Message::Other`], carried by number so it can be counted and skipped: the
//! header's length is what makes skipping safe.
//!
//! [MS-RDPEGFX] 2.2.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use super::wire::{Malformed, Reader, Writer};

/// The name the server opens the channel under.
pub const CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";

const WHAT: &str = "a graphics pipeline PDU";

/// `RDPGFX_HEADER`: cmdId, flags, pduLength.
const HEADER: u32 = 8;

/// `cmdId`s, [MS-RDPEGFX] 2.2.1.5.
pub const CMD_WIRE_TO_SURFACE_1: u16 = 0x0001;
pub const CMD_WIRE_TO_SURFACE_2: u16 = 0x0002;
pub const CMD_DELETE_ENCODING_CONTEXT: u16 = 0x0003;
pub const CMD_SOLID_FILL: u16 = 0x0004;
pub const CMD_SURFACE_TO_SURFACE: u16 = 0x0005;
pub const CMD_SURFACE_TO_CACHE: u16 = 0x0006;
pub const CMD_CACHE_TO_SURFACE: u16 = 0x0007;
pub const CMD_EVICT_CACHE_ENTRY: u16 = 0x0008;
pub const CMD_CREATE_SURFACE: u16 = 0x0009;
pub const CMD_DELETE_SURFACE: u16 = 0x000A;
pub const CMD_START_FRAME: u16 = 0x000B;
pub const CMD_END_FRAME: u16 = 0x000C;
pub const CMD_FRAME_ACKNOWLEDGE: u16 = 0x000D;
pub const CMD_RESET_GRAPHICS: u16 = 0x000E;
pub const CMD_MAP_SURFACE_TO_OUTPUT: u16 = 0x000F;
pub const CMD_CACHE_IMPORT_OFFER: u16 = 0x0010;
pub const CMD_CACHE_IMPORT_REPLY: u16 = 0x0011;
pub const CMD_CAPS_ADVERTISE: u16 = 0x0012;
pub const CMD_CAPS_CONFIRM: u16 = 0x0013;
pub const CMD_MAP_SURFACE_TO_WINDOW: u16 = 0x0015;
pub const CMD_QOE_FRAME_ACKNOWLEDGE: u16 = 0x0016;
pub const CMD_MAP_SURFACE_TO_SCALED_OUTPUT: u16 = 0x0017;
pub const CMD_MAP_SURFACE_TO_SCALED_WINDOW: u16 = 0x0018;

/// `codecId`s, [MS-RDPEGFX] 2.2.2.1 and 2.2.2.2.
pub const CODEC_UNCOMPRESSED: u16 = 0x0000;
pub const CODEC_CAVIDEO: u16 = 0x0003;
pub const CODEC_CLEARCODEC: u16 = 0x0008;
pub const CODEC_CAPROGRESSIVE: u16 = 0x0009;
pub const CODEC_PLANAR: u16 = 0x000A;
pub const CODEC_AVC420: u16 = 0x000B;
pub const CODEC_ALPHA: u16 = 0x000C;
pub const CODEC_CAPROGRESSIVE_V2: u16 = 0x000D;
pub const CODEC_AVC444: u16 = 0x000E;
pub const CODEC_AVC444_V2: u16 = 0x000F;

/// `RDPGFX_PIXELFORMAT`: 32 bits a pixel, blue first in memory, with the fourth
/// byte either unused or an alpha.
pub const PIXEL_XRGB_8888: u8 = 0x20;
pub const PIXEL_ARGB_8888: u8 = 0x21;

/// The capability sets this client advertises — [MS-RDPEGFX] 2.2.3.1 and 2.2.3.3 —
/// and the flags on them.
pub const CAPVERSION_8: u32 = 0x0008_0004;
pub const CAPVERSION_10: u32 = 0x000A_0002;
/// The client keeps the smaller bitmap cache: 4096 slots and 16 MiB, rather than
/// 25600 and 100 MiB.
pub const CAPS_SMALL_CACHE: u32 = 0x0000_0002;
/// The server is not to send H.264 in either of its shapes. Version 10 and later.
pub const CAPS_AVC_DISABLED: u32 = 0x0000_0020;

/// `queueDepth` in a frame acknowledgement: this client does not report how many
/// frames it holds undrawn, which the server takes as "send as you like".
pub const QUEUE_DEPTH_UNAVAILABLE: u32 = 0x0000_0000;

/// The largest surface or output dimension a server may name, from FreeRDP's own
/// bound: past it a `u16` field has wrapped or a server has lost its mind.
pub const MAX_DIMENSION: u32 = 32_766;

/// The most monitors a ResetGraphics may describe.
const MAX_MONITORS: u32 = 16;

/// `RECTANGLE_16`, with exclusive right and bottom edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect16 {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Rect16 {
    pub fn width(&self) -> u16 {
        self.right - self.left
    }

    pub fn height(&self) -> u16 {
        self.bottom - self.top
    }
}

/// `RDPGFX_POINT16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point16 {
    pub x: u16,
    pub y: u16,
}

/// What a server said in one PDU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// Pixels for one rectangle of a surface, in `codec`.
    WireToSurface1 { surface: u16, codec: u16, format: u8, rect: Rect16, data: &'a [u8] },
    /// Pixels for a surface in a codec that carries its own regions — RemoteFX
    /// Progressive — under an encoding context the server names.
    WireToSurface2 { surface: u16, codec: u16, context: u32, format: u8, data: &'a [u8] },
    DeleteEncodingContext { surface: u16, context: u32 },
    /// Fill these rectangles of a surface with one colour, `[b, g, r, a]`.
    SolidFill { surface: u16, color: [u8; 4], rects: Vec<Rect16> },
    /// Copy one rectangle of `src` to each point of `dst`.
    SurfaceToSurface { src: u16, dst: u16, rect: Rect16, points: Vec<Point16> },
    SurfaceToCache { surface: u16, key: u64, slot: u16, rect: Rect16 },
    CacheToSurface { slot: u16, surface: u16, points: Vec<Point16> },
    EvictCacheEntry { slot: u16 },
    CreateSurface { surface: u16, width: u16, height: u16, format: u8 },
    DeleteSurface { surface: u16 },
    StartFrame { timestamp: u32, frame: u32 },
    EndFrame { frame: u32 },
    /// The output is now this size. Answered to a monitor layout in place of a
    /// Deactivation-Reactivation Sequence.
    ResetGraphics { width: u32, height: u32, monitors: u32 },
    MapSurfaceToOutput { surface: u16, x: u32, y: u32 },
    /// The one capability set the server chose out of those advertised.
    CapsConfirm { version: u32, flags: u32 },
    /// A command this client has no reading for, skipped by its header's length.
    Other { command: u16, length: u32 },
}

/// The PDUs in one decompressed channel buffer, in order.
///
/// A PDU that does not decode ends the iteration: the buffer's remainder is at an
/// offset the failing header named, and nothing after a bad header is trustworthy.
pub fn messages(buffer: &[u8]) -> Messages<'_> {
    Messages { r: Reader::new(WHAT, buffer), failed: false }
}

pub struct Messages<'a> {
    r: Reader<'a>,
    failed: bool,
}

impl<'a> Iterator for Messages<'a> {
    type Item = Result<Message<'a>, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.r.is_empty() {
            return None;
        }
        let next = self.pdu();
        self.failed = next.is_err();
        Some(next)
    }
}

impl<'a> Messages<'a> {
    fn pdu(&mut self) -> Result<Message<'a>, Malformed> {
        let command = self.r.u16_le()?;
        let _flags = self.r.u16_le()?;
        let length = self.r.u32_le()?;
        let body = length
            .checked_sub(HEADER)
            .ok_or_else(|| self.r.refuse("a PDU length shorter than its header", length))?;
        let body = self.r.bytes(usize::try_from(body).unwrap_or(usize::MAX))?;
        let mut r = Reader::new(WHAT, body);
        let message = match command {
            CMD_WIRE_TO_SURFACE_1 => {
                let surface = r.u16_le()?;
                let codec = r.u16_le()?;
                let format = r.u8()?;
                let rect = rect16(&mut r)?;
                let length = r.u32_le()?;
                let data = r.bytes(usize::try_from(length).unwrap_or(usize::MAX))?;
                Message::WireToSurface1 { surface, codec, format, rect, data }
            }
            CMD_WIRE_TO_SURFACE_2 => {
                let surface = r.u16_le()?;
                let codec = r.u16_le()?;
                let context = r.u32_le()?;
                let format = r.u8()?;
                let length = r.u32_le()?;
                let data = r.bytes(usize::try_from(length).unwrap_or(usize::MAX))?;
                Message::WireToSurface2 { surface, codec, context, format, data }
            }
            CMD_DELETE_ENCODING_CONTEXT => {
                Message::DeleteEncodingContext { surface: r.u16_le()?, context: r.u32_le()? }
            }
            CMD_SOLID_FILL => {
                let surface = r.u16_le()?;
                let color = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
                let count = r.u16_le()?;
                let rects = (0..count).map(|_| rect16(&mut r)).collect::<Result<_, _>>()?;
                Message::SolidFill { surface, color, rects }
            }
            CMD_SURFACE_TO_SURFACE => {
                let src = r.u16_le()?;
                let dst = r.u16_le()?;
                let rect = rect16(&mut r)?;
                let count = r.u16_le()?;
                let points = (0..count).map(|_| point16(&mut r)).collect::<Result<_, _>>()?;
                Message::SurfaceToSurface { src, dst, rect, points }
            }
            CMD_SURFACE_TO_CACHE => {
                let surface = r.u16_le()?;
                let key = u64::from(r.u32_le()?) | (u64::from(r.u32_le()?) << 32);
                let slot = r.u16_le()?;
                let rect = rect16(&mut r)?;
                Message::SurfaceToCache { surface, key, slot, rect }
            }
            CMD_CACHE_TO_SURFACE => {
                let slot = r.u16_le()?;
                let surface = r.u16_le()?;
                let count = r.u16_le()?;
                let points = (0..count).map(|_| point16(&mut r)).collect::<Result<_, _>>()?;
                Message::CacheToSurface { slot, surface, points }
            }
            CMD_EVICT_CACHE_ENTRY => Message::EvictCacheEntry { slot: r.u16_le()? },
            CMD_CREATE_SURFACE => {
                let surface = r.u16_le()?;
                let width = r.u16_le()?;
                let height = r.u16_le()?;
                let format = r.u8()?;
                if format != PIXEL_XRGB_8888 && format != PIXEL_ARGB_8888 {
                    return Err(r.refuse("a surface pixel format", format));
                }
                if u32::from(width) > MAX_DIMENSION || u32::from(height) > MAX_DIMENSION {
                    return Err(r.refuse("a surface dimension", width.max(height)));
                }
                Message::CreateSurface { surface, width, height, format }
            }
            CMD_DELETE_SURFACE => Message::DeleteSurface { surface: r.u16_le()? },
            CMD_START_FRAME => Message::StartFrame { timestamp: r.u32_le()?, frame: r.u32_le()? },
            CMD_END_FRAME => Message::EndFrame { frame: r.u32_le()? },
            CMD_RESET_GRAPHICS => {
                let width = r.u32_le()?;
                let height = r.u32_le()?;
                let monitors = r.u32_le()?;
                if width > MAX_DIMENSION || height > MAX_DIMENSION {
                    return Err(r.refuse("an output dimension", width.max(height)));
                }
                if monitors > MAX_MONITORS {
                    return Err(r.refuse("a monitor count", monitors));
                }
                // The monitor definitions, and the padding that brings the PDU to
                // 340 bytes: neither is read, both have to be there.
                r.skip(monitors as usize * 20)?;
                Message::ResetGraphics { width, height, monitors }
            }
            CMD_MAP_SURFACE_TO_OUTPUT => {
                let surface = r.u16_le()?;
                r.u16_le()?; // reserved
                Message::MapSurfaceToOutput { surface, x: r.u32_le()?, y: r.u32_le()? }
            }
            CMD_CAPS_CONFIRM => {
                let version = r.u32_le()?;
                let length = r.u32_le()?;
                if length != 4 {
                    return Err(r.refuse("a capability data length", length));
                }
                Message::CapsConfirm { version, flags: r.u32_le()? }
            }
            other => Message::Other { command: other, length },
        };
        Ok(message)
    }
}

fn rect16(r: &mut Reader<'_>) -> Result<Rect16, Malformed> {
    let rect = Rect16 { left: r.u16_le()?, top: r.u16_le()?, right: r.u16_le()?, bottom: r.u16_le()? };
    if rect.left >= rect.right {
        return Err(r.refuse("a rectangle whose right edge is not past its left, at", rect.right));
    }
    if rect.top >= rect.bottom {
        return Err(r.refuse("a rectangle whose bottom edge is not past its top, at", rect.bottom));
    }
    Ok(rect)
}

fn point16(r: &mut Reader<'_>) -> Result<Point16, Malformed> {
    Ok(Point16 { x: r.u16_le()?, y: r.u16_le()? })
}

/// One PDU with its header, ready for [`super::zgfx::wrap`]. Crate-visible so the
/// compositor's tests can write a server's PDUs with it.
pub(crate) fn pdu(command: u16, body: &[u8]) -> Vec<u8> {
    let length = HEADER + u32::try_from(body.len()).expect("a client PDU is tens of bytes");
    let mut w = Writer::with_capacity(length as usize);
    w.u16_le(command);
    w.u16_le(0); // flags
    w.u32_le(length);
    w.bytes(body);
    w.finish()
}

/// What this client can take: version 8 and version 10, the small cache, and no
/// H.264.
///
/// Version 8 alone would have a Windows host draw with RemoteFX Progressive and
/// planar; version 10 adds nothing this client has to decode — its point is
/// `AVC_DISABLED`, which is a version-10 flag, so that a host which would otherwise
/// choose H.264 is told not to. `THINCLIENT` is deliberately not set: a current host
/// ignores it, and an older one would answer it with RemoteFX rather than the
/// progressive form.
pub fn caps_advertise() -> Vec<u8> {
    let sets = [(CAPVERSION_8, CAPS_SMALL_CACHE), (CAPVERSION_10, CAPS_SMALL_CACHE | CAPS_AVC_DISABLED)];
    let mut w = Writer::with_capacity(2 + sets.len() * 12);
    w.u16_le(u16::try_from(sets.len()).expect("two"));
    for (version, flags) in sets {
        w.u32_le(version);
        w.u32_le(4); // capsDataLength
        w.u32_le(flags);
    }
    pdu(CMD_CAPS_ADVERTISE, &w.finish())
}

/// The acknowledgement every EndFrame is owed. `decoded` is how many frames this
/// client has finished in all, which the server uses to notice a client that has
/// fallen behind.
pub fn frame_acknowledge(frame: u32, decoded: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(12);
    w.u32_le(QUEUE_DEPTH_UNAVAILABLE);
    w.u32_le(frame);
    w.u32_le(decoded);
    pdu(CMD_FRAME_ACKNOWLEDGE, &w.finish())
}

/// A codec's name, for a log line.
pub fn codec_name(codec: u16) -> &'static str {
    match codec {
        CODEC_UNCOMPRESSED => "Uncompressed",
        CODEC_CAVIDEO => "RemoteFX",
        CODEC_CLEARCODEC => "ClearCodec",
        CODEC_CAPROGRESSIVE => "Progressive",
        CODEC_PLANAR => "Planar",
        CODEC_AVC420 => "AVC420",
        CODEC_ALPHA => "Alpha",
        CODEC_CAPROGRESSIVE_V2 => "ProgressiveV2",
        CODEC_AVC444 => "AVC444",
        CODEC_AVC444_V2 => "AVC444v2",
        _ => "unknown",
    }
}

/// A command's name, for a log line.
pub fn command_name(command: u16) -> &'static str {
    match command {
        CMD_WIRE_TO_SURFACE_1 => "WireToSurface1",
        CMD_WIRE_TO_SURFACE_2 => "WireToSurface2",
        CMD_DELETE_ENCODING_CONTEXT => "DeleteEncodingContext",
        CMD_SOLID_FILL => "SolidFill",
        CMD_SURFACE_TO_SURFACE => "SurfaceToSurface",
        CMD_SURFACE_TO_CACHE => "SurfaceToCache",
        CMD_CACHE_TO_SURFACE => "CacheToSurface",
        CMD_EVICT_CACHE_ENTRY => "EvictCacheEntry",
        CMD_CREATE_SURFACE => "CreateSurface",
        CMD_DELETE_SURFACE => "DeleteSurface",
        CMD_START_FRAME => "StartFrame",
        CMD_END_FRAME => "EndFrame",
        CMD_FRAME_ACKNOWLEDGE => "FrameAcknowledge",
        CMD_RESET_GRAPHICS => "ResetGraphics",
        CMD_MAP_SURFACE_TO_OUTPUT => "MapSurfaceToOutput",
        CMD_CACHE_IMPORT_OFFER => "CacheImportOffer",
        CMD_CACHE_IMPORT_REPLY => "CacheImportReply",
        CMD_CAPS_ADVERTISE => "CapsAdvertise",
        CMD_CAPS_CONFIRM => "CapsConfirm",
        CMD_MAP_SURFACE_TO_WINDOW => "MapSurfaceToWindow",
        CMD_QOE_FRAME_ACKNOWLEDGE => "QoeFrameAcknowledge",
        CMD_MAP_SURFACE_TO_SCALED_OUTPUT => "MapSurfaceToScaledOutput",
        CMD_MAP_SURFACE_TO_SCALED_WINDOW => "MapSurfaceToScaledWindow",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server PDU, headed the way a server heads one.
    fn server(command: u16, body: &[u8]) -> Vec<u8> {
        pdu(command, body)
    }

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16_le(left);
        w.u16_le(top);
        w.u16_le(right);
        w.u16_le(bottom);
        w.finish()
    }

    fn one(bytes: &[u8]) -> Message<'_> {
        let mut all = messages(bytes);
        let message = all.next().expect("one PDU").expect("a well-formed PDU");
        assert!(all.next().is_none(), "exactly one PDU");
        message
    }

    /// The header's length is what delimits PDUs in a buffer that holds several,
    /// and what lets one this client does not read be stepped over.
    #[test]
    fn a_buffer_holds_several_pdus_and_an_unknown_one_is_stepped_over_by_length() {
        let mut buffer = server(CMD_START_FRAME, &[1, 0, 0, 0, 7, 0, 0, 0]);
        buffer.extend(server(0x0019, &[0xAA; 5])); // ProtectSurface, unread
        buffer.extend(server(CMD_END_FRAME, &[7, 0, 0, 0]));
        let decoded: Vec<_> = messages(&buffer).map(|m| m.unwrap()).collect();
        assert_eq!(decoded, vec![
            Message::StartFrame { timestamp: 1, frame: 7 },
            Message::Other { command: 0x0019, length: 13 },
            Message::EndFrame { frame: 7 },
        ]);
    }

    #[test]
    fn a_wire_to_surface_carries_its_rectangle_and_borrows_its_pixels() {
        let mut body = Writer::new();
        body.u16_le(3); // surface
        body.u16_le(CODEC_PLANAR);
        body.u8(PIXEL_XRGB_8888);
        body.bytes(&rect(10, 20, 14, 22));
        body.u32_le(3);
        body.bytes(&[9, 8, 7]);
        let bytes = server(CMD_WIRE_TO_SURFACE_1, &body.finish());
        let Message::WireToSurface1 { surface, codec, format, rect, data } = one(&bytes) else {
            panic!("a WireToSurface1");
        };
        assert_eq!((surface, codec, format), (3, CODEC_PLANAR, PIXEL_XRGB_8888));
        assert_eq!(rect, Rect16 { left: 10, top: 20, right: 14, bottom: 22 });
        assert_eq!((rect.width(), rect.height()), (4, 2));
        assert_eq!(data, &[9, 8, 7]);
    }

    #[test]
    fn a_reset_graphics_names_the_output_and_steps_over_its_monitors() {
        let mut body = Writer::new();
        body.u32_le(1600);
        body.u32_le(900);
        body.u32_le(1);
        body.zeros(20); // one monitor definition
        body.zeros(340 - 8 - 12 - 20); // padding to 340 bytes in all
        let bytes = server(CMD_RESET_GRAPHICS, &body.finish());
        assert_eq!(bytes.len(), 340);
        assert_eq!(one(&bytes), Message::ResetGraphics { width: 1600, height: 900, monitors: 1 });
    }

    #[test]
    fn the_surface_lifecycle_pdus_decode_to_their_fields() {
        let create = server(CMD_CREATE_SURFACE, &[1, 0, 0x00, 0x05, 0x84, 0x03, PIXEL_XRGB_8888]);
        assert_eq!(one(&create), Message::CreateSurface {
            surface: 1,
            width: 1280,
            height: 900,
            format: PIXEL_XRGB_8888
        });
        let map = server(CMD_MAP_SURFACE_TO_OUTPUT, &[1, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0, 0]);
        assert_eq!(one(&map), Message::MapSurfaceToOutput { surface: 1, x: 0, y: 32 });
        assert_eq!(one(&server(CMD_DELETE_SURFACE, &[1, 0])), Message::DeleteSurface { surface: 1 });
        let confirm = server(CMD_CAPS_CONFIRM, &[0x02, 0x00, 0x0A, 0x00, 4, 0, 0, 0, 0x22, 0, 0, 0]);
        assert_eq!(one(&confirm), Message::CapsConfirm { version: CAPVERSION_10, flags: 0x22 });
    }

    #[test]
    fn the_copy_and_cache_pdus_decode_their_lists() {
        let mut body = Writer::new();
        body.u16_le(1);
        body.bytes(&[10, 20, 30, 0xFF]);
        body.u16_le(2);
        body.bytes(&rect(0, 0, 4, 4));
        body.bytes(&rect(8, 8, 9, 9));
        let bytes = server(CMD_SOLID_FILL, &body.finish());
        let fill = one(&bytes);
        let Message::SolidFill { surface: 1, color: [10, 20, 30, 0xFF], rects } = fill else {
            panic!("{fill:?}");
        };
        assert_eq!(rects.len(), 2);

        let mut body = Writer::new();
        body.u16_le(1);
        body.u16_le(2);
        body.bytes(&rect(0, 0, 4, 4));
        body.u16_le(1);
        body.u16_le(100);
        body.u16_le(200);
        let bytes = server(CMD_SURFACE_TO_SURFACE, &body.finish());
        let copy = one(&bytes);
        let Message::SurfaceToSurface { src: 1, dst: 2, rect: _, points } = copy else {
            panic!("{copy:?}");
        };
        assert_eq!(points, vec![Point16 { x: 100, y: 200 }]);

        let mut body = Writer::new();
        body.u16_le(1);
        body.u32_le(0xDDCC_BBAA);
        body.u32_le(0x0403_0201);
        body.u16_le(7);
        body.bytes(&rect(0, 0, 4, 4));
        let bytes = server(CMD_SURFACE_TO_CACHE, &body.finish());
        let cache = one(&bytes);
        let Message::SurfaceToCache { key: 0x0403_0201_DDCC_BBAA, slot: 7, .. } = cache else {
            panic!("{cache:?}");
        };
        assert_eq!(one(&server(CMD_EVICT_CACHE_ENTRY, &[7, 0])), Message::EvictCacheEntry { slot: 7 });
    }

    /// A rectangle with nothing inside it is not a rectangle a server draws.
    #[test]
    fn an_empty_rectangle_is_refused() {
        let mut body = Writer::new();
        body.u16_le(1);
        body.u16_le(CODEC_UNCOMPRESSED);
        body.u8(PIXEL_XRGB_8888);
        body.bytes(&rect(10, 20, 10, 22));
        body.u32_le(0);
        let bytes = server(CMD_WIRE_TO_SURFACE_1, &body.finish());
        let err = messages(&bytes).next().unwrap().unwrap_err();
        assert!(matches!(err, Malformed::Refused { value: 10, .. }), "{err}");
    }

    /// The iteration ends at a header that does not fit or a length that lies: what
    /// follows is at an offset nothing vouches for.
    #[test]
    fn a_bad_header_ends_the_buffer() {
        let mut buffer = server(CMD_END_FRAME, &[7, 0, 0, 0]);
        buffer.extend([CMD_END_FRAME as u8, 0, 0, 0, 4, 0, 0, 0]); // a length under the header
        buffer.extend(server(CMD_END_FRAME, &[8, 0, 0, 0]));
        let mut all = messages(&buffer);
        assert!(all.next().unwrap().is_ok());
        let err = all.next().unwrap().unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "a PDU length shorter than its header", .. }));
        assert!(all.next().is_none(), "nothing after a bad header is read");

        let mut truncated = server(CMD_END_FRAME, &[7, 0, 0, 0]);
        truncated.pop();
        let err = messages(&truncated).next().unwrap().unwrap_err();
        assert!(matches!(err, Malformed::Short { .. }));
    }

    /// The two PDUs this client writes, byte for byte.
    #[test]
    fn the_caps_advertise_and_the_frame_acknowledge_are_written_whole() {
        let caps = caps_advertise();
        let mut r = Reader::new("a test", &caps);
        assert_eq!(r.u16_le().unwrap(), CMD_CAPS_ADVERTISE);
        assert_eq!(r.u16_le().unwrap(), 0);
        assert_eq!(r.u32_le().unwrap() as usize, caps.len());
        assert_eq!(r.u16_le().unwrap(), 2);
        assert_eq!(
            (r.u32_le().unwrap(), r.u32_le().unwrap(), r.u32_le().unwrap()),
            (CAPVERSION_8, 4, CAPS_SMALL_CACHE)
        );
        assert_eq!(
            (r.u32_le().unwrap(), r.u32_le().unwrap(), r.u32_le().unwrap()),
            (CAPVERSION_10, 4, CAPS_SMALL_CACHE | CAPS_AVC_DISABLED)
        );
        assert!(r.is_empty());

        assert_eq!(frame_acknowledge(7, 3), vec![
            0x0D, 0x00, 0x00, 0x00, 20, 0, 0, 0, // header
            0, 0, 0, 0, // queueDepth
            7, 0, 0, 0, // frameId
            3, 0, 0, 0, // totalFramesDecoded
        ]);
    }
}
