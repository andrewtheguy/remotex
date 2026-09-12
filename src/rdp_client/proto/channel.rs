//! The header a static virtual channel's data wears inside an MCS Send Data.
//!
//! [`super::mcs`] addresses a payload to a channel; this is what sits at the front of
//! that payload on every channel but the I/O one. It is eight bytes — a length and a
//! set of flags — and it exists for one reason: an MCS Send Data is capped at what
//! PER's length field holds, and a channel PDU may be larger, so a long one is split
//! into chunks that each carry the whole PDU's length and say where in it they fall.
//!
//! The chunk size is the server's to choose. It announces one in the Virtual Channel
//! capability set, between [`MIN_CHUNK`] and [`MAX_CHUNK`], and a client that writes
//! a larger chunk than that is not answered — it is disconnected.
//!
//! # Both directions carry a PDU of any length
//!
//! Two channels are asked for, and the clipboard is the one whose PDUs are as long as
//! somebody's copy: half a megabyte of text is a megabyte of UTF-16, against a chunk
//! floor of [`MIN_CHUNK`]. So [`chunks`] splits what it is given, and [`Reassembly`]
//! puts a server's pieces back together.
//!
//! What neither of them does is hold a PDU of unbounded size for a peer. Outbound is
//! the gateway's own text and already bounded by
//! [`MAX_CLIPBOARD_BYTES`](crate::protocol::MAX_CLIPBOARD_BYTES); inbound is whatever
//! the remote copied, so a PDU announcing more than [`MAX_PDU`] is read past and
//! dropped — [`Chunk::Dropped`] — rather than gathered. A novel on the far end's
//! clipboard is a thing that happens, not a protocol violation, and ending the
//! session over one would be losing a desktop to a copy.
//!
//! [MS-RDPBCGR] 2.2.6.1.

use super::wire::{Malformed, Reader, Writer};

/// The smallest chunk a server may name, which is also what to assume when it names
/// none.
pub const MIN_CHUNK: usize = 1600;

/// The largest chunk a server may name.
pub const MAX_CHUNK: usize = 16_256;

/// The most a whole inbound PDU may come to before it is dropped instead of held.
///
/// Four megabytes, which is four times the largest clipboard this gateway will
/// carry — [`MAX_CLIPBOARD_BYTES`](crate::protocol::MAX_CLIPBOARD_BYTES) of text is a
/// megabyte of UTF-16 — and so leaves room for a copy that is refused as oversized
/// *after* it arrives, which is how the browser gets told a size rather than a
/// silence. Past that the bytes are of no use to anyone here, and the only question is
/// whether reading them costs memory the server chose.
pub const MAX_PDU: usize = 4 << 20;

/// The length and flags in front of every chunk.
const HEADER: usize = 8;

/// `CHANNEL_FLAG_FIRST`: this chunk starts a PDU.
const FIRST: u32 = 0x0000_0001;
/// `CHANNEL_FLAG_LAST`: this chunk ends one.
const LAST: u32 = 0x0000_0002;
/// `CHANNEL_FLAG_SHOW_PROTOCOL`: these eight bytes are to stay visible to whatever
/// reads the data, rather than being stripped off in front of it.
///
/// It belongs to the *channel*, not to the PDU: a channel whose `CS_NET` options asked
/// for it wears it on every chunk it ever sends, and one that did not never wears it —
/// [`super::gcc::Channel::chunk_flags`] is where that is decided, so the two can never
/// disagree.
///
/// Both halves are load-bearing, and neither is a guess. Against a Windows host the
/// server endpoint reads *nothing at all* off a chunk whose header disagrees with the
/// option: the clipboard ignored every PDU this client sent until this flag was set,
/// and the dynamic channel ignored every PDU — never opening Display Control — as soon
/// as it was. Nothing is reported either way; the far end simply stops answering.
pub const SHOW_PROTOCOL: u32 = 0x0000_0010;
/// `CHANNEL_PACKET_COMPRESSED`: the data is compressed, and the length is what it
/// will decompress to. [`super::capabilities`] asks for no compression in either
/// direction, so a server that sets this is refused rather than half-read.
const COMPRESSED: u32 = 0x0020_0000;

const WHAT: &str = "a virtual channel PDU";

/// A payload longer than a virtual channel carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a {length}-byte channel PDU is more than the {} MiB one carries", MAX_PDU >> 20)]
pub struct TooLong {
    pub length: usize,
}

/// One channel PDU as the chunks it is sent in, each headed and ready for one MCS
/// Send Data.
///
/// Every chunk announces the whole PDU's length and says where in it it falls, so the
/// far end knows both how much is coming and when it has all arrived. A payload that
/// fits is one chunk; an empty one is still a chunk, because an empty PDU — a format
/// list advertising nothing — is something this client really sends.
///
/// `channel` is what this channel's chunks always wear beyond those two markers, which
/// is [`SHOW_PROTOCOL`] or nothing — see [`super::gcc::Channel::chunk_flags`].
pub fn chunks(payload: &[u8], chunk: usize, channel: u32) -> Result<Vec<Vec<u8>>, TooLong> {
    if payload.len() > MAX_PDU {
        return Err(TooLong { length: payload.len() });
    }
    let total = u32::try_from(payload.len()).expect("a length MAX_PDU just bounded");
    let chunk = chunk.max(1); // a server that named nothing gets MIN_CHUNK; belt and braces
    let pieces = payload.chunks(chunk);
    let count = pieces.len().max(1);
    let mut written = Vec::with_capacity(count);
    for (at, piece) in pieces.chain(payload.is_empty().then_some(&[][..])).enumerate() {
        let mut flags = channel;
        if at == 0 {
            flags |= FIRST;
        }
        if at + 1 == count {
            flags |= LAST;
        }
        let mut w = Writer::with_capacity(HEADER + piece.len());
        w.u32_le(total);
        w.u32_le(flags);
        w.bytes(piece);
        written.push(w.finish());
    }
    Ok(written)
}

/// What one chunk did to the PDU it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chunk<'a> {
    /// Not the last piece; there is more of this PDU coming.
    Partial,
    /// One whole PDU. It borrows either the chunk it arrived in — which is every PDU
    /// a Display Control session sees — or the buffer its pieces were gathered into.
    Whole(&'a [u8]),
    /// A PDU past [`MAX_PDU`], read to its end and dropped. `length` is what it
    /// announced, which is all that is left of it, and is reported so a caller that
    /// was waiting for it can say what happened instead of waiting on.
    Dropped { length: usize },
}

/// The chunks of a channel PDU, put back together.
///
/// One of these belongs to each channel: the pieces of two PDUs never interleave on
/// one channel, and a second channel's chunks are a separate sequence.
#[derive(Debug, Default)]
pub struct Reassembly {
    buffer: Vec<u8>,
    /// The length the PDU in progress announced, and the length it has to come to.
    expected: Option<usize>,
    /// The length a PDU too long to hold announced, while its chunks are read past.
    dropping: Option<usize>,
}

impl Reassembly {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one chunk, and say what it did to the PDU it belongs to.
    pub fn push<'a>(&'a mut self, chunk: &'a [u8]) -> Result<Chunk<'a>, Malformed> {
        let mut r = Reader::new(WHAT, chunk);
        let announced = r.u32_le()?;
        let total = usize::try_from(announced).unwrap_or(usize::MAX);
        let flags = r.u32_le()?;
        let data = r.rest();

        if flags & COMPRESSED != 0 {
            return Err(self.abandon("compressed channel data, whose flags are", flags));
        }
        let (first, last) = (flags & FIRST != 0, flags & LAST != 0);
        let unfinished = "a first chunk inside an unfinished PDU, whose flags are";

        // A PDU being read past. Nothing in its chunks will be acted on, so the only
        // thing left to get right is where it ends — a chunk of it taken for the
        // start of the next PDU would make that one nonsense too.
        if let Some(length) = self.dropping {
            if first {
                return Err(self.abandon(unfinished, flags));
            }
            if !last {
                return Ok(Chunk::Partial);
            }
            self.dropping = None;
            return Ok(Chunk::Dropped { length });
        }

        if first {
            if self.expected.is_some() {
                return Err(self.abandon(unfinished, flags));
            }
            // Longer than this client holds for a peer — see [`MAX_PDU`]. Read past
            // rather than refused: the connection is fine, and it is only this PDU
            // that is of no use.
            if total > MAX_PDU {
                if last {
                    return Ok(Chunk::Dropped { length: total });
                }
                self.dropping = Some(total);
                return Ok(Chunk::Partial);
            }
            if last {
                if data.len() != total {
                    return Err(self.abandon("a whole PDU whose announced length is", announced));
                }
                return Ok(Chunk::Whole(data));
            }
            self.expected = Some(total);
            self.buffer.clear();
            self.buffer.extend_from_slice(data);
            return Ok(Chunk::Partial);
        }

        let Some(expected) = self.expected else {
            return Err(self.abandon("a chunk that continues nothing, whose flags are", flags));
        };
        if total != expected {
            return Err(self.abandon("a chunk announcing a different length,", announced));
        }
        self.buffer.extend_from_slice(data);
        if self.buffer.len() > expected || (last && self.buffer.len() != expected) {
            let gathered = u32::try_from(self.buffer.len()).unwrap_or(u32::MAX);
            return Err(self.abandon("chunks coming to", gathered));
        }
        if !last {
            return Ok(Chunk::Partial);
        }
        self.expected = None;
        Ok(Chunk::Whole(&self.buffer))
    }

    /// Forget what was being reassembled, and say why it was given up on. A sequence
    /// that went wrong cannot be continued: the chunks after it belong to a PDU whose
    /// start is gone.
    fn abandon(&mut self, field: &'static str, value: impl Into<u64>) -> Malformed {
        self.buffer.clear();
        self.expected = None;
        self.dropping = None;
        Malformed::Refused { what: WHAT, field, value: value.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chunk as a server writes one, for feeding to [`Reassembly`].
    fn chunk(total: usize, flags: u32, data: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32_le(u32::try_from(total).unwrap());
        w.u32_le(flags);
        w.bytes(data);
        w.finish()
    }

    #[test]
    fn a_pdu_that_fits_one_chunk_is_written_whole() {
        let written = chunks(b"hello", MIN_CHUNK, 0).unwrap();
        assert_eq!(written, vec![chunk(5, FIRST | LAST, b"hello")]);
    }

    /// A clipboard PDU is as long as somebody's copy, so it is split — each piece
    /// announcing the whole length, and only the ends carrying the end flags.
    #[test]
    fn a_pdu_longer_than_a_chunk_is_split_into_pieces_that_name_the_whole() {
        let payload: Vec<u8> = (0..MIN_CHUNK * 2 + 7).map(|at| at as u8).collect();
        let written = chunks(&payload, MIN_CHUNK, 0).unwrap();
        assert_eq!(written.len(), 3);
        let total = payload.len();
        assert_eq!(written[0], chunk(total, FIRST, &payload[..MIN_CHUNK]));
        assert_eq!(written[1], chunk(total, 0, &payload[MIN_CHUNK..MIN_CHUNK * 2]));
        assert_eq!(written[2], chunk(total, LAST, &payload[MIN_CHUNK * 2..]));

        // And what was split reassembles into what went in, which is the whole point
        // of the length every piece carries.
        let mut reassembly = Reassembly::new();
        for (at, piece) in written.iter().enumerate() {
            match reassembly.push(piece).unwrap() {
                Chunk::Whole(pdu) => {
                    assert_eq!(at, 2, "the PDU completed early");
                    assert_eq!(pdu, &payload[..]);
                }
                Chunk::Partial => assert_ne!(at, 2, "the last piece did not complete it"),
                Chunk::Dropped { .. } => panic!("a PDU this size is held, not dropped"),
            }
        }
    }

    /// A PDU that exactly fills its chunks takes no fourth, empty one — a peer
    /// counting bytes would be waiting for a piece that never comes.
    #[test]
    fn a_pdu_that_divides_evenly_is_split_with_no_empty_tail() {
        let written = chunks(&[0; MIN_CHUNK * 2], MIN_CHUNK, 0).unwrap();
        assert_eq!(written.len(), 2);
        assert_eq!(&written[1][..8], &chunk(MIN_CHUNK * 2, LAST, &[])[..8]);
    }

    /// An empty PDU is one this client really sends — a format list advertising
    /// nothing — so it is one chunk carrying no data rather than no chunk at all.
    #[test]
    fn an_empty_payload_is_still_one_chunk() {
        assert_eq!(chunks(&[], MIN_CHUNK, 0).unwrap(), vec![chunk(0, FIRST | LAST, &[])]);
    }

    /// The flag a channel was opened with rides on every chunk of every PDU, first,
    /// middle and last. A Windows host reads nothing off a chunk that disagrees with
    /// the channel — see [`SHOW_PROTOCOL`].
    #[test]
    fn a_channels_own_flag_rides_on_every_chunk_of_every_pdu() {
        let payload = vec![7_u8; MIN_CHUNK + 1];
        let written = chunks(&payload, MIN_CHUNK, SHOW_PROTOCOL).unwrap();
        assert_eq!(written.len(), 2);
        let flags = |piece: &[u8]| u32::from_le_bytes(piece[4..8].try_into().unwrap());
        assert_eq!(flags(&written[0]), FIRST | SHOW_PROTOCOL);
        assert_eq!(flags(&written[1]), LAST | SHOW_PROTOCOL);

        // And a channel that asked for nothing carries nothing, which is just as
        // load-bearing: the same host stops answering a channel that wears it
        // uninvited.
        let plain = chunks(&payload, MIN_CHUNK, 0).unwrap();
        assert_eq!(flags(&plain[0]), FIRST);
        assert_eq!(flags(&plain[1]), LAST);
    }

    #[test]
    fn a_payload_longer_than_a_channel_carries_is_refused_rather_than_sent() {
        let err = chunks(&vec![0; MAX_PDU + 1], MIN_CHUNK, 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a 4194305-byte channel PDU is more than the 4 MiB one carries"
        );
    }

    #[test]
    fn a_whole_pdu_arrives_borrowed_from_the_chunk_it_came_in() {
        let mut reassembly = Reassembly::new();
        let bytes = chunk(3, FIRST | LAST, &[1, 2, 3]);
        assert_eq!(reassembly.push(&bytes).unwrap(), Chunk::Whole(&[1, 2, 3][..]));
    }

    #[test]
    fn the_pieces_of_a_long_pdu_are_gathered_until_the_last_one() {
        let mut reassembly = Reassembly::new();
        assert_eq!(reassembly.push(&chunk(6, FIRST, &[1, 2])).unwrap(), Chunk::Partial);
        assert_eq!(reassembly.push(&chunk(6, 0, &[3, 4])).unwrap(), Chunk::Partial);
        assert_eq!(
            reassembly.push(&chunk(6, LAST, &[5, 6])).unwrap(),
            Chunk::Whole(&[1, 2, 3, 4, 5, 6][..])
        );

        // And the reassembler is ready for the next PDU rather than holding the last.
        assert_eq!(reassembly.push(&chunk(1, FIRST | LAST, &[7])).unwrap(), Chunk::Whole(&[7][..]));
    }

    /// A copy on the far end can be any size, and the session survives one that is
    /// larger than this client will hold: the PDU is read to its end, reported as
    /// dropped once, and the channel carries on.
    #[test]
    fn a_pdu_too_long_to_hold_is_read_past_rather_than_ending_the_session() {
        let mut reassembly = Reassembly::new();
        let huge = MAX_PDU + 1;
        assert_eq!(reassembly.push(&chunk(huge, FIRST, &[0; 4])).unwrap(), Chunk::Partial);
        assert_eq!(reassembly.push(&chunk(huge, 0, &[0; 4])).unwrap(), Chunk::Partial);
        assert_eq!(
            reassembly.push(&chunk(huge, LAST, &[0; 4])).unwrap(),
            Chunk::Dropped { length: huge }
        );
        // Nothing was gathered, and the next PDU is read as a PDU.
        assert_eq!(reassembly.push(&chunk(1, FIRST | LAST, &[7])).unwrap(), Chunk::Whole(&[7][..]));
    }

    /// The chunk sizes a server may name cannot carry one of these in a single
    /// chunk, so this is the arithmetic being right rather than a case on the wire.
    #[test]
    fn a_single_chunk_claiming_to_be_too_long_is_dropped_where_it_stands() {
        let mut reassembly = Reassembly::new();
        let length = MAX_PDU + 1;
        let piece = chunk(length, FIRST | LAST, &[0; 8]);
        assert_eq!(reassembly.push(&piece).unwrap(), Chunk::Dropped { length });
        assert_eq!(reassembly.push(&chunk(1, FIRST | LAST, &[7])).unwrap(), Chunk::Whole(&[7][..]));
    }

    #[test]
    fn a_sequence_that_does_not_add_up_is_refused_and_given_up_on() {
        let mut reassembly = Reassembly::new();
        assert_eq!(reassembly.push(&chunk(6, FIRST, &[1, 2])).unwrap(), Chunk::Partial);
        let err = reassembly.push(&chunk(6, LAST, &[3])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a virtual channel PDU carries chunks coming to 0x3, which this client does not accept"
        );

        // The sequence is gone, so the next chunk has to start one.
        let err = reassembly.push(&chunk(6, LAST, &[4])).unwrap_err();
        assert!(matches!(
            err,
            Malformed::Refused { field: "a chunk that continues nothing, whose flags are", .. }
        ));
    }

    #[test]
    fn a_second_start_inside_an_unfinished_pdu_is_refused() {
        let mut reassembly = Reassembly::new();
        assert_eq!(reassembly.push(&chunk(6, FIRST, &[1, 2])).unwrap(), Chunk::Partial);
        let err = reassembly.push(&chunk(2, FIRST | LAST, &[1, 2])).unwrap_err();
        let field = "a first chunk inside an unfinished PDU, whose flags are";
        assert!(matches!(err, Malformed::Refused { field: refused, .. } if refused == field));
    }

    /// No compression was claimed, so compressed data is not something to try to
    /// read — the length in the header is not even the length of what follows.
    #[test]
    fn compressed_channel_data_is_refused_by_name() {
        let mut reassembly = Reassembly::new();
        let err = reassembly.push(&chunk(2, FIRST | LAST | COMPRESSED, &[1])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a virtual channel PDU carries compressed channel data, whose flags are 0x200003, \
             which this client does not accept"
        );
    }

    #[test]
    fn a_chunk_too_short_for_its_own_header_is_short_rather_than_a_panic() {
        let mut reassembly = Reassembly::new();
        assert!(matches!(reassembly.push(&[0; 7]).unwrap_err(), Malformed::Short { .. }));
    }
}
