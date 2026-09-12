//! The framing a server's updates arrive in, once the desktop is live.
//!
//! Everything up to the Font Map travels the slow path: a TPKT frame, an X.224 data
//! TPDU, an MCS Send Data Indication and a share control header — twenty-odd bytes of
//! envelope around a PDU that is often smaller than that. From the first update
//! onwards a server that was told this client understands fast-path output drops all
//! of it and sends a two-byte header instead.
//!
//! Both framings then share the socket: updates come fast-path, and a Deactivate All
//! or a Set Error Info still arrives the slow way. Which is which is decided by the
//! first byte, and decided without ambiguity — a TPKT frame begins with its version,
//! 3, and a fast-path PDU begins with a byte whose bottom two bits are the action,
//! which in the server's direction is 0.
//!
//! # One PDU, several updates; one update, several PDUs
//!
//! A fast-path PDU carries a run of updates, each with a type and its own length, so
//! a server may put a bitmap and a pointer shape in one frame. It works the other way
//! as well: a frame is at most 32 kilobytes, because the length field is fifteen bits,
//! and a full-screen update is far larger than that — so one update may be cut into
//! pieces marked first, next and last, and put back together here.
//!
//! The Multifragment Update capability is the two ends agreeing how large that
//! reassembled whole may be. [`Fragments`] is where the number is enforced, because
//! the buffer it bounds is this gateway's memory and the server is the one filling it.
//!
//! [MS-RDPBCGR] 2.2.9.1.2.

use super::wire::{Malformed, Reader};

/// The bottom two bits of the first byte, which are the action.
const ACTION: u8 = 0x03;

/// `FASTPATH_OUTPUT_ACTION_FASTPATH`: this is an update rather than an X.224 frame.
const ACTION_FASTPATH: u8 = 0x00;

/// `FASTPATH_OUTPUT_SECURE_CHECKSUM` and `FASTPATH_OUTPUT_ENCRYPTED`: the PDU carries
/// a signature, or is encrypted by RDP's own security rather than by TLS. This client
/// refuses every security method that would set either — see [`super::gcc`] — so one
/// of them here means the connection is not the one that was negotiated.
const SECURE: u8 = 0x40 | 0x80;

/// The top bit of the first length byte: the length is two bytes rather than one.
const LONG_LENGTH: u8 = 0x80;

/// The header of one update: the type, the fragmentation, and the compression.
const CODE: u8 = 0x0F;
const FRAGMENT: u8 = 0x30;
const COMPRESSION: u8 = 0xC0;

/// `FASTPATH_OUTPUT_COMPRESSION_USED`, which means a compression flags byte follows
/// the update header. The flags may still say the body was left alone.
const COMPRESSION_USED: u8 = 0x40;

/// `PACKET_COMPRESSED`: the body really is compressed. A server may only compress
/// what the Client Info PDU asked it to, and [`super::info`] asks for nothing.
const PACKET_COMPRESSED: u8 = 0x20;

/// Update types. Everything this client can be sent is here, including the ones it
/// refuses, so that a refusal can name what arrived rather than a number.
pub const ORDERS: u8 = 0x0;
pub const BITMAP: u8 = 0x1;
pub const PALETTE: u8 = 0x2;
pub const SYNCHRONIZE: u8 = 0x3;
pub const SURFACE_COMMANDS: u8 = 0x4;
pub const POINTER_HIDDEN: u8 = 0x5;
pub const POINTER_DEFAULT: u8 = 0x6;
pub const POINTER_POSITION: u8 = 0x8;
pub const COLOR_POINTER: u8 = 0x9;
pub const CACHED_POINTER: u8 = 0xA;
pub const NEW_POINTER: u8 = 0xB;
pub const LARGE_POINTER: u8 = 0xC;

/// Where a PDU's updates start, and how long the whole PDU is.
struct Header {
    start: usize,
    length: usize,
}

/// Whether the frame beginning with this byte is a fast-path output PDU rather than
/// the TPKT framing everything else still uses.
pub fn is_output(first: u8) -> bool {
    first & ACTION == ACTION_FASTPATH
}

/// How long the fast-path PDU beginning at `bytes` is in total, its own header
/// included.
///
/// `None` while that cannot be known yet, which is until two bytes are in hand — or
/// three, when the second says the length is a long one. A caller reads what it is
/// short of and asks again; there is never a fourth byte to wait for.
pub fn frame_length(bytes: &[u8]) -> Result<Option<usize>, Malformed> {
    Ok(header(bytes)?.map(|header| header.length))
}

fn header(bytes: &[u8]) -> Result<Option<Header>, Malformed> {
    const WHAT: &str = "a fast-path output header";

    let mut r = Reader::new(WHAT, bytes);
    let Ok(first) = r.u8() else { return Ok(None) };
    if first & SECURE != 0 {
        return Err(r.refuse("its security flags", first));
    }
    let Ok(short) = r.u8() else { return Ok(None) };
    let length = if short & LONG_LENGTH == 0 {
        u16::from(short)
    } else {
        let Ok(rest) = r.u8() else { return Ok(None) };
        u16::from_be_bytes([short & !LONG_LENGTH, rest])
    };
    // The length counts the header it is part of, so a PDU claiming less than it has
    // already spent is one no reader can go on from.
    if usize::from(length) < r.at() {
        return Err(r.refuse("its length", length));
    }
    Ok(Some(Header { start: r.at(), length: usize::from(length) }))
}

/// Whether this piece is a whole update, or where it sits in one that was cut up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fragment {
    /// `FASTPATH_FRAGMENT_SINGLE`.
    Single,
    /// `FASTPATH_FRAGMENT_LAST`.
    Last,
    /// `FASTPATH_FRAGMENT_FIRST`.
    First,
    /// `FASTPATH_FRAGMENT_NEXT`.
    Next,
}

/// One update, or one piece of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece<'a> {
    /// One of the update types above.
    pub code: u8,
    pub fragment: Fragment,
    pub data: &'a [u8],
}

/// One whole update, however many pieces it arrived in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Update<'a> {
    pub code: u8,
    pub data: &'a [u8],
}

/// Every update in one whole fast-path output PDU, in order.
pub fn updates(frame: &[u8]) -> Result<Updates<'_>, Malformed> {
    const WHAT: &str = "a fast-path output PDU";

    let Some(Header { start, length }) = header(frame)? else {
        return Err(Malformed::Short { what: WHAT, len: frame.len(), at: frame.len(), need: 1 });
    };
    let mut r = Reader::new(WHAT, frame);
    // The declared length is what the PDU is. Reading to the end of the buffer
    // instead would hand on whatever a caller read past it.
    r.skip(start)?;
    let updates = r.bytes(length - start)?;
    Ok(Updates { r: Reader::new("a fast-path update", updates) })
}

/// The iterator [`updates`] hands back.
pub struct Updates<'a> {
    r: Reader<'a>,
}

impl<'a> Iterator for Updates<'a> {
    type Item = Result<Piece<'a>, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.r.is_empty() {
            return None;
        }
        Some(self.piece())
    }
}

impl<'a> Updates<'a> {
    fn piece(&mut self) -> Result<Piece<'a>, Malformed> {
        let header = self.r.u8()?;
        match header & COMPRESSION {
            0 => {}
            COMPRESSION_USED => {
                let flags = self.r.u8()?;
                if flags & PACKET_COMPRESSED != 0 {
                    return Err(self.r.refuse("its compression flags", flags));
                }
            }
            other => return Err(self.r.refuse("its compression", other)),
        }
        let length = self.r.u16_le()?;
        let data = self.r.bytes(usize::from(length))?;
        let fragment = match header & FRAGMENT {
            0x00 => Fragment::Single,
            0x10 => Fragment::Last,
            0x20 => Fragment::First,
            _ => Fragment::Next,
        };
        Ok(Piece { code: header & CODE, fragment, data })
    }
}

/// An update being put back together out of pieces.
///
/// One at a time: [MS-RDPBCGR] does not let a server begin a second update before it
/// has finished the first, and a client that allowed it would be holding an unbounded
/// number of half-updates on a server's say-so.
pub struct Fragments {
    /// The type of the update in progress, taken from its first piece. The pieces
    /// after it carry one too, and it is not read: what an update is was settled when
    /// it began.
    code: Option<u8>,
    buffer: Vec<u8>,
    limit: usize,
}

impl Fragments {
    /// `limit` is the Multifragment Update capability the two ends settled on: the
    /// most bytes one reassembled update may come to.
    pub fn new(limit: u32) -> Self {
        Self { code: None, buffer: Vec::new(), limit: limit.try_into().unwrap_or(usize::MAX) }
    }

    /// Add one piece, and hand back the update if this piece completed it.
    ///
    /// A whole update borrows either the piece — which is nearly every update, since
    /// most are far smaller than a frame — or the buffer its pieces were gathered
    /// into.
    pub fn push<'a>(&'a mut self, piece: Piece<'a>) -> Result<Option<Update<'a>>, Malformed> {
        match piece.fragment {
            Fragment::Single if self.code.is_some() => {
                Err(self.abandon("a whole update inside an unfinished one", piece.code))
            }
            Fragment::Single => Ok(Some(Update { code: piece.code, data: piece.data })),
            Fragment::First if self.code.is_some() => {
                Err(self.abandon("a first fragment inside an unfinished update", piece.code))
            }
            Fragment::First => {
                self.code = Some(piece.code);
                self.buffer.clear();
                self.gather(piece.data)?;
                Ok(None)
            }
            Fragment::Next | Fragment::Last => {
                let Some(code) = self.code else {
                    return Err(self.abandon("a fragment that continues nothing", piece.code));
                };
                self.gather(piece.data)?;
                if piece.fragment == Fragment::Next {
                    return Ok(None);
                }
                self.code = None;
                Ok(Some(Update { code, data: &self.buffer }))
            }
        }
    }

    fn gather(&mut self, data: &[u8]) -> Result<(), Malformed> {
        let total = self.buffer.len() + data.len();
        if total > self.limit {
            return Err(self.abandon_long(total));
        }
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    /// Forget what was being reassembled, and say why it was given up on.
    ///
    /// The connection ends on this error, so the state matters only in that nothing
    /// half-read should outlive the sentence that explains it.
    fn abandon(&mut self, field: &'static str, code: u8) -> Malformed {
        self.forget();
        Reader::new("a fast-path update", &[]).refuse(field, code)
    }

    fn abandon_long(&mut self, total: usize) -> Malformed {
        self.forget();
        Reader::new("a reassembled fast-path update", &[])
            .refuse("a total length", u64::try_from(total).unwrap_or(u64::MAX))
    }

    fn forget(&mut self) {
        self.code = None;
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fast-path output PDU carrying each `(update header, body)` in order.
    fn pdu(updates: &[(u8, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (header, data) in updates {
            body.push(*header);
            body.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
            body.extend_from_slice(data);
        }
        wrap(&body)
    }

    /// A fast-path output PDU around updates that are already written out.
    fn wrap(body: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x00];
        let short = 2 + body.len();
        if short < 0x80 {
            frame.push(u8::try_from(short).unwrap());
        } else {
            let long = u16::try_from(short + 1).unwrap();
            frame.extend_from_slice(&(long | u16::from(LONG_LENGTH) << 8).to_be_bytes());
        }
        frame.extend_from_slice(body);
        frame
    }

    /// The two framings share the socket, and one byte tells them apart.
    #[test]
    fn a_tpkt_frame_is_not_mistaken_for_an_update() {
        assert!(is_output(0x00));
        assert!(!is_output(0x03), "a TPKT version byte");
    }

    #[test]
    fn a_frame_announces_its_whole_length_including_its_own_header() {
        let frame = pdu(&[(BITMAP, &[0xAB; 8])]);
        assert_eq!(frame.len(), 2 + 3 + 8);
        assert_eq!(frame_length(&frame).unwrap(), Some(frame.len()));

        // Past 127 bytes the length takes a second byte, and the frame grows by it.
        let frame = pdu(&[(BITMAP, &[0xAB; 200])]);
        assert_eq!(frame.len(), 3 + 3 + 200);
        assert_eq!(frame_length(&frame).unwrap(), Some(frame.len()));
    }

    /// A caller reads what it is short of and asks again, rather than being told the
    /// buffer ended — the buffer is meant to end, and the length says where.
    #[test]
    fn a_length_that_is_not_all_there_yet_is_not_an_error() {
        let frame = pdu(&[(BITMAP, &[0xAB; 200])]);
        assert_eq!(frame_length(&[]).unwrap(), None);
        assert_eq!(frame_length(&frame[..1]).unwrap(), None);
        assert_eq!(frame_length(&frame[..2]).unwrap(), None, "the long length needs a third byte");
        assert_eq!(frame_length(&frame[..3]).unwrap(), Some(frame.len()));
    }

    /// RDP's own encryption is refused in the negotiation, so a PDU that claims it is
    /// a connection that is not what it was negotiated to be.
    #[test]
    fn an_encrypted_or_signed_pdu_is_refused_rather_than_read_past() {
        let mut frame = pdu(&[(BITMAP, &[0; 4])]);
        frame[0] = 0x80;
        assert_eq!(
            frame_length(&frame).unwrap_err().to_string(),
            "a fast-path output header carries its security flags 0x80, which this client does \
             not accept"
        );
    }

    #[test]
    fn every_update_in_one_pdu_comes_out_in_order() {
        let frame = pdu(&[(BITMAP, &[0x01, 0x02]), (POINTER_POSITION, &[0x03, 0x04])]);
        let updates: Vec<_> = updates(&frame).unwrap().map(Result::unwrap).collect();
        assert_eq!(updates, vec![
            Piece { code: BITMAP, fragment: Fragment::Single, data: &[0x01, 0x02] },
            Piece { code: POINTER_POSITION, fragment: Fragment::Single, data: &[0x03, 0x04] },
        ]);
    }

    /// The declared length is what the PDU is. A caller that read past it — off a
    /// stream, that is every caller — must not be handed the bytes after it.
    #[test]
    fn the_declared_length_bounds_the_updates_rather_than_the_buffer() {
        let mut frame = pdu(&[(BITMAP, &[0x01, 0x02])]);
        frame.extend_from_slice(&[SYNCHRONIZE, 0x00, 0x00]);
        assert_eq!(updates(&frame).unwrap().count(), 1);
    }

    #[test]
    fn a_compressed_update_is_refused_and_an_uncompressed_one_wearing_the_flag_is_not() {
        // The flag says a byte of compression flags follows, and those flags say the
        // body was left alone after all.
        let frame = wrap(&[BITMAP | COMPRESSION_USED, 0x00, 0x02, 0x00, 0xAB, 0xCD]);
        let piece = updates(&frame).unwrap().next().unwrap().unwrap();
        assert_eq!(piece, Piece { code: BITMAP, fragment: Fragment::Single, data: &[0xAB, 0xCD] });

        // And when they say it really was compressed, it is refused: a server may
        // only compress what the Client Info PDU asked it to.
        let frame = wrap(&[BITMAP | COMPRESSION_USED, PACKET_COMPRESSED, 0x02, 0x00, 0xAB, 0xCD]);
        assert_eq!(
            updates(&frame).unwrap().next().unwrap().unwrap_err().to_string(),
            "a fast-path update carries its compression flags 0x20, which this client does not \
             accept"
        );
    }

    /// The pieces of one update, in the order a server sends them.
    fn pieces<'a>(code: u8, parts: &[&'a [u8]]) -> Vec<Piece<'a>> {
        let last = parts.len() - 1;
        parts
            .iter()
            .enumerate()
            .map(|(at, data)| {
                let fragment = match at {
                    0 => Fragment::First,
                    at if at == last => Fragment::Last,
                    _ => Fragment::Next,
                };
                Piece { code, fragment, data }
            })
            .collect()
    }

    #[test]
    fn an_update_cut_into_pieces_comes_back_whole() {
        let mut fragments = Fragments::new(64);
        let parts: [&[u8]; 3] = [&[0x01, 0x02], &[0x03], &[0x04, 0x05]];
        let mut whole = Vec::new();
        for piece in pieces(BITMAP, &parts) {
            if let Some(update) = fragments.push(piece).unwrap() {
                whole.push((update.code, update.data.to_vec()));
            }
        }
        assert_eq!(whole, vec![(BITMAP, vec![0x01, 0x02, 0x03, 0x04, 0x05])]);

        // And the buffer is the update's, not a running total: a second one that
        // follows carries only its own bytes.
        let parts: [&[u8]; 2] = [&[0x06], &[0x07]];
        let mut whole = Vec::new();
        for piece in pieces(BITMAP, &parts) {
            if let Some(update) = fragments.push(piece).unwrap() {
                whole.push(update.data.to_vec());
            }
        }
        assert_eq!(whole, vec![vec![0x06, 0x07]]);
    }

    #[test]
    fn an_update_that_was_never_cut_up_is_handed_straight_back() {
        let mut fragments = Fragments::new(64);
        let piece = Piece { code: POINTER_HIDDEN, fragment: Fragment::Single, data: &[] };
        assert_eq!(fragments.push(piece).unwrap(), Some(Update { code: POINTER_HIDDEN, data: &[] }));
    }

    #[test]
    fn a_fragment_that_continues_nothing_ends_the_connection() {
        let mut fragments = Fragments::new(64);
        let piece = Piece { code: BITMAP, fragment: Fragment::Last, data: &[0x01] };
        assert_eq!(
            fragments.push(piece).unwrap_err().to_string(),
            "a fast-path update carries a fragment that continues nothing 0x1, which this client \
             does not accept"
        );
    }

    /// Interleaving is not something [MS-RDPBCGR] allows, and allowing it here would
    /// mean holding as many half-updates as a server cares to begin.
    #[test]
    fn an_update_beginning_inside_another_ends_the_connection() {
        let mut fragments = Fragments::new(64);
        fragments
            .push(Piece { code: BITMAP, fragment: Fragment::First, data: &[0x01] })
            .unwrap();
        let piece = Piece { code: COLOR_POINTER, fragment: Fragment::Single, data: &[0x02] };
        assert_eq!(
            fragments.push(piece).unwrap_err().to_string(),
            "a fast-path update carries a whole update inside an unfinished one 0x9, which this \
             client does not accept"
        );
    }

    /// The limit is the Multifragment Update capability, and the memory it bounds is
    /// this gateway's.
    #[test]
    fn a_reassembled_update_larger_than_the_two_ends_agreed_is_refused() {
        let mut fragments = Fragments::new(4);
        fragments
            .push(Piece { code: BITMAP, fragment: Fragment::First, data: &[0x01, 0x02, 0x03] })
            .unwrap();
        let piece = Piece { code: BITMAP, fragment: Fragment::Next, data: &[0x04, 0x05] };
        assert_eq!(
            fragments.push(piece).unwrap_err().to_string(),
            "a reassembled fast-path update carries a total length 0x5, which this client does \
             not accept"
        );
    }
}
