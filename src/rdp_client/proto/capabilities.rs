//! What the two ends can do, exchanged once before any pixel moves.
//!
//! The server opens a share with a **Demand Active** PDU: a share identifier, and a
//! list of capability sets describing what it will send. The client answers with a
//! **Confirm Active** carrying its own list, and from then on each side may only send
//! what the other's list allowed. Getting this wrong does not fail loudly — it fails
//! several seconds later as a PDU nobody can decode — so every set this client sends
//! is written out here in full rather than being defaulted.
//!
//! # What is claimed, and what is not
//!
//! The short version is that this client takes **bitmaps and nothing else**. It
//! claims no drawing orders, no bitmap cache, no glyph cache, no offscreen cache and
//! no brushes, so a server has nothing to send but whole rectangles of pixels. That
//! is more bytes on a LAN and far less code on both sides of the decode, and a gateway
//! re-encodes everything for the browser anyway: an order replayed into a local
//! framebuffer buys nothing downstream.
//!
//! It does claim fast-path output, which is the compact framing every modern server
//! prefers, and it asks not to be sent the compression header in front of a bitmap —
//! the update already says how long the data is.
//!
//! # Reading the server's list
//!
//! Three things in the server's list are read. The desktop size it decided on, which
//! may not be the size that was asked for; how large a reassembled fast-path update
//! may be; and the colour depth, which is the one field here that is refused rather
//! than recorded — [`super::bitmap`] decodes a 32-bit session and only that, so a
//! server that opened a shallower one is a sentence during the capability exchange
//! instead of a desktop whose pixels cannot be read. Everything else is stepped over
//! by its own declared length, which is what lets a server announce capabilities from
//! a decade this client has never heard of without ending the connection.

use super::bitmap;
use super::channel;
use super::wire::{Malformed, Reader, Writer};

/// `originatorId`, which [MS-RDPBCGR] requires to be the server's own MCS channel.
/// mstsc echoes the Demand Active's `pduSource` instead, and servers accept either;
/// this is the one the specification names.
const SERVER_CHANNEL: u16 = 0x03EA;

/// `sourceDescriptor`. Ignored by every server, logged by some.
const SOURCE: &[u8] = b"remotex\0";

/// Capability set types.
const GENERAL: u16 = 0x01;
const BITMAP: u16 = 0x02;
const ORDER: u16 = 0x03;
const BITMAP_CACHE: u16 = 0x04;
const POINTER: u16 = 0x08;
const SOUND: u16 = 0x0C;
const INPUT: u16 = 0x0D;
const BRUSH: u16 = 0x0F;
const GLYPH_CACHE: u16 = 0x10;
const OFFSCREEN_CACHE: u16 = 0x11;
const VIRTUAL_CHANNEL: u16 = 0x14;
const MULTIFRAGMENT: u16 = 0x1A;
const LARGE_POINTER: u16 = 0x1B;

/// A capability set's own header: type and length, the length counting itself.
const SET_HEADER: u16 = 4;

/// `TS_CAPS_PROTOCOLVERSION`, which has one value.
const PROTOCOL_VERSION: u16 = 0x0200;

/// The platform this gateway runs on, as the General capability spells it. A server
/// records it and acts on none of it.
#[cfg(windows)]
const PLATFORM: u16 = 1;
#[cfg(target_os = "macos")]
const PLATFORM: u16 = 3;
#[cfg(not(any(windows, target_os = "macos")))]
const PLATFORM: u16 = 4;

/// `extraFlags`:
///
/// - `0x0001` fast-path output is understood, so the server may use the short framing
///   instead of wrapping every update in MCS.
/// - `0x0400` do not put a compression header in front of a bitmap: the update
///   already carries the length, and the header is eight bytes of nothing.
const EXTRA_FLAGS: u16 = 0x0001 | 0x0400;

/// `drawingFlags`: `DRAW_ALLOW_SKIP_ALPHA`, which lets the server leave the unused
/// byte of a 32-bit pixel alone instead of writing an alpha into it.
const DRAWING_FLAGS: u8 = 0x08;

/// What this client renders, and what it asks the desktop to be.
const COLOR_DEPTH: u16 = bitmap::DEPTH;

/// `orderFlags`: `NEGOTIATEORDERSUPPORT`, so the empty support array below is read as
/// the refusal it is, and `ZEROBOUNDSDELTASSUPPORT`, which every client sets.
const ORDER_FLAGS: u16 = 0x0002 | 0x0008;

/// `desktopSaveYGranularity`, which is 20 whether or not anything saves a desktop.
const SAVE_Y_GRANULARITY: u16 = 20;

/// `inputFlags`: scancodes, extended mouse buttons, both fast-path input revisions,
/// Unicode keystrokes and a horizontal wheel. Not relative mouse motion, which needs
/// a pointer this client does not have, and not QoE timestamps, which nothing reads.
const INPUT_FLAGS: u16 = 0x0001 | 0x0004 | 0x0008 | 0x0010 | 0x0020 | 0x0100;

/// `keyboardType`: the IBM enhanced 101/102-key keyboard, and the twelve function
/// keys that go with it.
const KEYBOARD_TYPE: u32 = 4;
const FUNCTION_KEYS: u32 = 12;

/// `imeFileName`, which is 64 bytes whether or not there is an input method.
const IME_FILE_NAME: usize = 64;

/// How many pointer shapes the server may assume are held. Zero would mean the server
/// must resend a shape on every change, which for a blinking text caret is a shape per
/// blink.
const POINTER_CACHE: u16 = 32;

/// `largePointerSupportFlags`: pointers up to 96x96 and up to 384x384. The first is
/// what older hosts check; the second is what a high-density desktop needs.
const LARGE_POINTER_FLAGS: u16 = 0x0001 | 0x0002;

/// What to ask for when the server names no maximum of its own: eight megabytes, which
/// is more than a full-screen uncompressed update at any size this client opens.
const DEFAULT_MULTIFRAGMENT: u32 = 8 * 1024 * 1024;

/// What the server said when it opened the share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemandActive {
    /// Names the share, and every data PDU either side sends from now on carries it.
    pub share_id: u32,
    /// The desktop the server settled on, which is not always the one that was asked
    /// for: a server clamps to what its own session can be.
    pub width: u16,
    pub height: u16,
    /// The largest fast-path update the server will reassemble into one, out of its
    /// Multifragment Update capability.
    pub multifragment: u32,
    /// The largest chunk a virtual channel PDU may be split into, out of the server's
    /// Virtual Channel capability. A server that names none is taken to mean the
    /// smallest, which is what [MS-RDPBCGR] says every server accepts.
    pub chunk: usize,
}

impl DemandActive {
    /// Read a Demand Active PDU, given the body of its share control header.
    pub fn decode(body: &[u8]) -> Result<Self, Malformed> {
        const WHAT: &str = "an RDP Demand Active PDU";

        let mut r = Reader::new(WHAT, body);
        let share_id = r.u32_le()?;
        let descriptor = r.u16_le()?;
        let _combined = r.u16_le()?;
        r.skip(usize::from(descriptor))?;
        let count = r.u16_le()?;
        r.skip(2)?;

        let mut desktop = None;
        let mut multifragment = None;
        let mut chunk = None;
        for _ in 0..count {
            let kind = r.u16_le()?;
            let length = r.u16_le()?;
            let body = match length.checked_sub(SET_HEADER) {
                Some(body) => r.bytes(usize::from(body))?,
                None => return Err(r.refuse("a capability set length", length)),
            };
            match kind {
                BITMAP => {
                    let mut r = Reader::new("a server Bitmap capability set", body);
                    let depth = r.u16_le()?;
                    if depth != COLOR_DEPTH {
                        return Err(r.refuse("a colour depth", depth));
                    }
                    // The three flags for the depths an ancient client could receive.
                    r.skip(6)?;
                    desktop = Some((r.u16_le()?, r.u16_le()?));
                }
                MULTIFRAGMENT => {
                    let mut r = Reader::new("a server Multifragment Update capability set", body);
                    multifragment = Some(r.u32_le()?);
                }
                VIRTUAL_CHANNEL => {
                    let mut r = Reader::new("a server Virtual Channel capability set", body);
                    // `flags`, which say what compression the server would use. This
                    // client claims none, so a server has nothing to compress with.
                    r.skip(4)?;
                    // `VCChunkSize`, which older servers leave off the end.
                    if r.is_empty() {
                        continue;
                    }
                    let named = r.u32_le()?;
                    let size = usize::try_from(named).unwrap_or(usize::MAX);
                    if !(channel::MIN_CHUNK..=channel::MAX_CHUNK).contains(&size) {
                        return Err(r.refuse("a channel chunk size", named));
                    }
                    chunk = Some(size);
                }
                _ => {}
            }
        }

        let (width, height) = desktop.ok_or_else(|| r.missing("a Bitmap capability set"))?;
        Ok(Self {
            share_id,
            width,
            height,
            multifragment: multifragment.unwrap_or(DEFAULT_MULTIFRAGMENT),
            chunk: chunk.unwrap_or(channel::MIN_CHUNK),
        })
    }
}

/// What this client answers with.
pub struct ConfirmActive {
    /// The share the server named.
    pub share_id: u32,
    /// The desktop the server named. Echoed rather than re-asked: the size was
    /// negotiated in the GCC conference and settled by the Demand Active.
    pub width: u16,
    pub height: u16,
    /// The keyboard layout named in the GCC conference, repeated because the Input
    /// capability has a field for it and a server may read either.
    pub keyboard_layout: u32,
    /// The largest fast-path update this client will be sent in one piece — the
    /// server's own number, handed back.
    pub multifragment: u32,
}

impl ConfirmActive {
    /// The body of a Confirm Active PDU, for [`super::share::confirm_active`].
    pub fn encode(&self) -> Vec<u8> {
        let sets = self.sets();
        let mut w = Writer::with_capacity(12 + SOURCE.len() + sets.len());
        w.u32_le(self.share_id);
        w.u16_le(SERVER_CHANNEL);
        w.u16_le(u16::try_from(SOURCE.len()).expect("a source descriptor of eight bytes"));
        w.u16_le(
            u16::try_from(sets.len() + 4).expect("this client's capability list is under a kilobyte"),
        );
        w.bytes(SOURCE);
        w.u16_le(COUNT);
        w.zeros(2);
        w.bytes(&sets);
        w.finish()
    }

    fn sets(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(384);

        header(&mut w, GENERAL, 24);
        w.u16_le(PLATFORM);
        // The minor platform, the protocol version, and a pad.
        w.u16_le(0);
        w.u16_le(PROTOCOL_VERSION);
        w.zeros(2);
        // No bulk compression, which is decided in the Client Info PDU and repeated
        // here as the absence it is.
        w.u16_le(0);
        w.u16_le(EXTRA_FLAGS);
        // updateCapabilityFlag, remoteUnshareFlag and generalCompressionLevel, all of
        // which are required to be zero.
        w.zeros(6);
        // Refresh Rect and Suppress Output: neither is asked for, because a gateway
        // that wants a repaint has a framebuffer of its own to repaint from.
        w.u8(0);
        w.u8(0);

        header(&mut w, BITMAP, 28);
        w.u16_le(COLOR_DEPTH);
        // The 1-, 4- and 8-bit receive flags, which have to say yes and mean nothing.
        w.u16_le(1);
        w.u16_le(1);
        w.u16_le(1);
        w.u16_le(self.width);
        w.u16_le(self.height);
        w.zeros(2);
        // The desktop may be resized under this client, which is what lets Display
        // Control work at all.
        w.u16_le(1);
        // Bitmaps may arrive compressed, and in more than one rectangle at a time.
        w.u16_le(1);
        w.u8(0);
        w.u8(DRAWING_FLAGS);
        w.u16_le(1);
        w.zeros(2);

        header(&mut w, ORDER, 88);
        // terminalDescriptor, and the pad after it.
        w.zeros(16);
        w.zeros(4);
        w.u16_le(1);
        w.u16_le(SAVE_Y_GRANULARITY);
        w.zeros(2);
        // maximumOrderLevel, and no fonts.
        w.u16_le(1);
        w.u16_le(0);
        w.u16_le(ORDER_FLAGS);
        // The support array: one byte per order, and every one of them a refusal.
        w.zeros(32);
        w.u16_le(0);
        w.u16_le(0);
        w.zeros(4);
        // No desktop is saved, so no space is set aside to save it in.
        w.u32_le(0);
        w.zeros(4);
        w.u16_le(0);
        w.zeros(2);

        // A cache of no entries, which is how a client says it caches nothing.
        header(&mut w, BITMAP_CACHE, 40);
        w.zeros(36);

        header(&mut w, POINTER, 10);
        // Colour pointers are understood — the alternative is a two-colour mask.
        w.u16_le(1);
        w.u16_le(POINTER_CACHE);
        w.u16_le(POINTER_CACHE);

        header(&mut w, INPUT, 88);
        w.u16_le(INPUT_FLAGS);
        w.zeros(2);
        w.u32_le(self.keyboard_layout);
        w.u32_le(KEYBOARD_TYPE);
        // No keyboard subtype: the enhanced keyboard has none.
        w.u32_le(0);
        w.u32_le(FUNCTION_KEYS);
        w.zeros(IME_FILE_NAME);

        header(&mut w, BRUSH, 8);
        // Solid brushes only, which is all a client that draws no orders can be sent.
        w.u32_le(0);

        header(&mut w, GLYPH_CACHE, 52);
        // Ten glyph caches and a fragment cache, all of no entries, and glyphs not
        // supported at all.
        w.zeros(48);

        header(&mut w, OFFSCREEN_CACHE, 12);
        w.zeros(8);

        header(&mut w, VIRTUAL_CHANNEL, 12);
        // No compression in either direction, and no chunk size of this client's own:
        // the server's is the one that governs.
        w.u32_le(0);
        w.u32_le(0);

        header(&mut w, SOUND, 8);
        // No beeps. Audio is refused in the Client Info PDU and refused again here.
        w.u16_le(0);
        w.zeros(2);

        header(&mut w, LARGE_POINTER, 6);
        w.u16_le(LARGE_POINTER_FLAGS);

        header(&mut w, MULTIFRAGMENT, 8);
        w.u32_le(self.multifragment);

        w.finish()
    }
}

/// How many sets [`ConfirmActive::sets`] writes.
const COUNT: u16 = 13;

fn header(w: &mut Writer, kind: u16, length: u16) {
    w.u16_le(kind);
    w.u16_le(length);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirm() -> ConfirmActive {
        ConfirmActive {
            share_id: 0x0001_0021,
            width: 1920,
            height: 1080,
            keyboard_layout: 0x0409,
            multifragment: 0x0004_0000,
        }
    }

    /// Every set announces its own length, and a reader that trusts those lengths has
    /// to land exactly on the end of the list.
    #[test]
    fn every_capability_set_announces_the_length_it_occupies() {
        let sets = confirm().sets();
        let mut r = Reader::new("the list", &sets);
        let mut seen = Vec::new();
        while !r.is_empty() {
            let kind = r.u16_le().unwrap();
            let length = r.u16_le().unwrap();
            r.skip(usize::from(length) - 4).unwrap();
            seen.push((kind, length));
        }
        assert_eq!(
            seen,
            vec![
                (GENERAL, 24),
                (BITMAP, 28),
                (ORDER, 88),
                (BITMAP_CACHE, 40),
                (POINTER, 10),
                (INPUT, 88),
                (BRUSH, 8),
                (GLYPH_CACHE, 52),
                (OFFSCREEN_CACHE, 12),
                (VIRTUAL_CHANNEL, 12),
                (SOUND, 8),
                (LARGE_POINTER, 6),
                (MULTIFRAGMENT, 8),
            ]
        );
        assert_eq!(seen.len(), usize::from(COUNT));
    }

    #[test]
    fn the_pdu_announces_the_list_that_follows_it() {
        let bytes = confirm().encode();
        let mut r = Reader::new("the PDU", &bytes);
        assert_eq!(r.u32_le().unwrap(), 0x0001_0021);
        assert_eq!(r.u16_le().unwrap(), SERVER_CHANNEL);
        let descriptor = r.u16_le().unwrap();
        let combined = r.u16_le().unwrap();
        assert_eq!(usize::from(descriptor), SOURCE.len());
        r.skip(usize::from(descriptor)).unwrap();
        assert_eq!(r.u16_le().unwrap(), COUNT);
        r.skip(2).unwrap();
        // The combined length covers the count, the pad, and the sets themselves.
        assert_eq!(usize::from(combined), 4 + r.rest().len());
    }

    #[test]
    fn nothing_that_would_bring_an_order_or_a_cache_is_claimed() {
        let sets = confirm().sets();
        let at = 24 + 28 + 4;
        // Every byte of the order support array is a refusal.
        assert!(sets[at + 32..at + 64].iter().all(|byte| *byte == 0));
        // And the bitmap cache that follows holds nothing.
        let cache = 24 + 28 + 88 + 4;
        assert!(sets[cache..cache + 36].iter().all(|byte| *byte == 0));
    }

    /// The server's own list, as a Windows host sends it: more sets than this client
    /// reads, in an order it does not choose.
    fn demand(sets: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32_le(0x0001_0021);
        w.u16_le(4);
        w.u16_le(
            u16::try_from(4 + sets.iter().map(|(_, body)| body.len() + 4).sum::<usize>()).unwrap(),
        );
        w.bytes(b"RDP\0");
        w.u16_le(u16::try_from(sets.len()).unwrap());
        w.zeros(2);
        for (kind, body) in sets {
            w.u16_le(*kind);
            w.u16_le(u16::try_from(body.len() + 4).unwrap());
            w.bytes(body);
        }
        w.u32_le(0);
        w.finish()
    }

    fn bitmap(width: u16, height: u16) -> (u16, Vec<u8>) {
        let mut w = Writer::new();
        w.u16_le(COLOR_DEPTH);
        w.zeros(6);
        w.u16_le(width);
        w.u16_le(height);
        w.zeros(12);
        (BITMAP, w.finish())
    }

    #[test]
    fn the_desktop_the_server_settled_on_is_the_one_that_comes_back() {
        // Asked for 1920x1080, given 1024x768, with sets either side that this client
        // has no use for and must still step over.
        let pdu = demand(&[
            (GENERAL, vec![0; 20]),
            bitmap(1024, 768),
            (ORDER, vec![0; 84]),
            (MULTIFRAGMENT, 0x0004_0000_u32.to_le_bytes().to_vec()),
            (VIRTUAL_CHANNEL, [0_u32.to_le_bytes(), 16_256_u32.to_le_bytes()].concat()),
            (0x1D, vec![0; 40]),
        ]);
        assert_eq!(
            DemandActive::decode(&pdu).unwrap(),
            DemandActive {
                share_id: 0x0001_0021,
                width: 1024,
                height: 768,
                multifragment: 0x0004_0000,
                chunk: 16_256,
            }
        );
    }

    /// Two fields a server may leave off the end, and what this client does without
    /// them: ask for as much as it can take, and send as little as every server takes.
    #[test]
    fn a_server_that_names_neither_size_gets_one_asked_of_it_and_one_assumed() {
        let pdu = demand(&[bitmap(1920, 1080), (VIRTUAL_CHANNEL, 0_u32.to_le_bytes().to_vec())]);
        let demanded = DemandActive::decode(&pdu).unwrap();
        assert_eq!(demanded.multifragment, DEFAULT_MULTIFRAGMENT);
        assert_eq!(demanded.chunk, channel::MIN_CHUNK);
    }

    /// A chunk outside the range the specification gives is a server this client
    /// cannot write to: too small and a PDU it sends is refused, too large and the
    /// number is not one a server meant.
    #[test]
    fn a_chunk_size_no_server_should_name_is_refused_where_it_is_named() {
        for named in [1599_u32, 16_257] {
            let chunk = [0_u32.to_le_bytes(), named.to_le_bytes()].concat();
            let pdu = demand(&[bitmap(1920, 1080), (VIRTUAL_CHANNEL, chunk)]);
            assert_eq!(
                DemandActive::decode(&pdu).unwrap_err().to_string(),
                format!(
                    "a server Virtual Channel capability set carries a channel chunk size \
                     {named:#x}, which this client does not accept"
                )
            );
        }
    }

    #[test]
    fn a_share_with_no_desktop_in_it_is_an_error_rather_than_a_guess() {
        let pdu = demand(&[(GENERAL, vec![0; 20])]);
        assert_eq!(
            DemandActive::decode(&pdu).unwrap_err().to_string(),
            "an RDP Demand Active PDU does not carry a Bitmap capability set"
        );
    }

    /// The session's depth is the server's to decide, and a shallower one is a
    /// decoder that cannot read a pixel — so it is refused here, where the server says
    /// so, rather than later where a bitmap would.
    #[test]
    fn a_session_that_is_not_32_bit_is_refused_where_the_server_declares_it() {
        let mut pdu = demand(&[bitmap(1920, 1080)]);
        let at = 4 + 2 + 2 + 4 + 2 + 2 + 4;
        pdu[at..at + 2].copy_from_slice(&16_u16.to_le_bytes());
        assert_eq!(
            DemandActive::decode(&pdu).unwrap_err().to_string(),
            "a server Bitmap capability set carries a colour depth 0x10, which this client does \
             not accept"
        );
    }

    #[test]
    fn a_capability_set_shorter_than_its_own_header_is_refused() {
        let mut pdu = demand(&[bitmap(1920, 1080)]);
        // The length field of the one set, made too small to contain itself.
        let at = 4 + 2 + 2 + 4 + 2 + 2 + 2;
        pdu[at..at + 2].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            DemandActive::decode(&pdu).unwrap_err().to_string(),
            "an RDP Demand Active PDU carries a capability set length 0x2, which this client does \
             not accept"
        );
    }
}
