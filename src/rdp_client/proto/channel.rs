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
//! # One chunk out, many in
//!
//! Everything this client sends on a virtual channel is a Display Control monitor
//! layout: a fixed sixty-four bytes, wrapped in a dynamic channel header of two or
//! three. Nothing it can send comes near [`MIN_CHUNK`], so [`pdu`] writes one chunk
//! and refuses a payload that would need a second, rather than carrying a splitter
//! that no call site reaches.
//!
//! Inbound is the other way round: what a server sends is its decision, so
//! [`Reassembly`] puts the pieces of any length back together.
//!
//! [MS-RDPBCGR] 2.2.6.1.

use super::wire::{Malformed, Reader, Writer};

/// The smallest chunk a server may name, which is also what to assume when it names
/// none.
pub const MIN_CHUNK: usize = 1600;

/// The largest chunk a server may name.
pub const MAX_CHUNK: usize = 16_256;

/// The length and flags in front of every chunk.
const HEADER: usize = 8;

/// `CHANNEL_FLAG_FIRST`: this chunk starts a PDU.
const FIRST: u32 = 0x0000_0001;
/// `CHANNEL_FLAG_LAST`: this chunk ends one.
const LAST: u32 = 0x0000_0002;
/// `CHANNEL_PACKET_COMPRESSED`: the data is compressed, and the length is what it
/// will decompress to. [`super::capabilities`] asks for no compression in either
/// direction, so a server that sets this is refused rather than half-read.
const COMPRESSED: u32 = 0x0020_0000;

const WHAT: &str = "a virtual channel PDU";

/// A payload longer than one chunk of the channel it was addressed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a {length}-byte channel PDU does not fit the server's {chunk}-byte chunk")]
pub struct TooLong {
    pub length: usize,
    pub chunk: usize,
}

/// One whole channel PDU, headed and ready for one MCS Send Data.
pub fn pdu(payload: &[u8], chunk: usize) -> Result<Vec<u8>, TooLong> {
    if payload.len() > chunk {
        return Err(TooLong { length: payload.len(), chunk });
    }
    let mut w = Writer::with_capacity(HEADER + payload.len());
    w.u32_le(u32::try_from(payload.len()).expect("a length the chunk size just bounded"));
    w.u32_le(FIRST | LAST);
    w.bytes(payload);
    Ok(w.finish())
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
}

impl Reassembly {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one chunk, and hand back the PDU if this chunk completed it.
    ///
    /// A whole PDU borrows either the chunk — which is every PDU a Display Control
    /// session sees — or the buffer its chunks were gathered into.
    pub fn push<'a>(&'a mut self, chunk: &'a [u8]) -> Result<Option<&'a [u8]>, Malformed> {
        let mut r = Reader::new(WHAT, chunk);
        let announced = r.u32_le()?;
        let total = usize::try_from(announced).unwrap_or(usize::MAX);
        let flags = r.u32_le()?;
        let data = r.rest();

        if flags & COMPRESSED != 0 {
            return Err(self.abandon("compressed channel data, whose flags are", flags));
        }
        let (first, last) = (flags & FIRST != 0, flags & LAST != 0);

        if first {
            if self.expected.is_some() {
                let what = "a first chunk inside an unfinished PDU, whose flags are";
                return Err(self.abandon(what, flags));
            }
            if last {
                if data.len() != total {
                    return Err(self.abandon("a whole PDU whose announced length is", announced));
                }
                return Ok(Some(data));
            }
            self.expected = Some(total);
            self.buffer.clear();
            self.buffer.extend_from_slice(data);
            return Ok(None);
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
            return Ok(None);
        }
        self.expected = None;
        Ok(Some(&self.buffer))
    }

    /// Forget what was being reassembled, and say why it was given up on. A sequence
    /// that went wrong cannot be continued: the chunks after it belong to a PDU whose
    /// start is gone.
    fn abandon(&mut self, field: &'static str, value: impl Into<u64>) -> Malformed {
        self.buffer.clear();
        self.expected = None;
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
        let bytes = pdu(b"hello", MIN_CHUNK).unwrap();
        assert_eq!(bytes, chunk(5, FIRST | LAST, b"hello"));
    }

    /// Nothing this client sends is near a chunk, so the splitter that would be
    /// needed is an error instead — a loud one, rather than a truncated PDU.
    #[test]
    fn a_pdu_too_long_for_one_chunk_is_refused_rather_than_split() {
        let err = pdu(&[0; MIN_CHUNK + 1], MIN_CHUNK).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a 1601-byte channel PDU does not fit the server's 1600-byte chunk"
        );
    }

    #[test]
    fn a_whole_pdu_arrives_borrowed_from_the_chunk_it_came_in() {
        let mut reassembly = Reassembly::new();
        let bytes = chunk(3, FIRST | LAST, &[1, 2, 3]);
        assert_eq!(reassembly.push(&bytes).unwrap(), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn the_pieces_of_a_long_pdu_are_gathered_until_the_last_one() {
        let mut reassembly = Reassembly::new();
        assert_eq!(reassembly.push(&chunk(6, FIRST, &[1, 2])).unwrap(), None);
        assert_eq!(reassembly.push(&chunk(6, 0, &[3, 4])).unwrap(), None);
        assert_eq!(
            reassembly.push(&chunk(6, LAST, &[5, 6])).unwrap(),
            Some(&[1, 2, 3, 4, 5, 6][..])
        );

        // And the reassembler is ready for the next PDU rather than holding the last.
        assert_eq!(reassembly.push(&chunk(1, FIRST | LAST, &[7])).unwrap(), Some(&[7][..]));
    }

    #[test]
    fn a_sequence_that_does_not_add_up_is_refused_and_given_up_on() {
        let mut reassembly = Reassembly::new();
        assert_eq!(reassembly.push(&chunk(6, FIRST, &[1, 2])).unwrap(), None);
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
        assert_eq!(reassembly.push(&chunk(6, FIRST, &[1, 2])).unwrap(), None);
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
