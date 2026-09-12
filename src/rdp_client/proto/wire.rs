//! Reading and writing the numbers a PDU is made of.
//!
//! Every RDP structure is a run of fixed-width integers and byte strings, and the
//! two bugs that live at this layer are always the same: reading past the end of a
//! buffer, and reading a field in the wrong byte order. [`Reader`] answers the first
//! by making the end of the buffer a `Result` rather than a panic. Both answer the
//! second by putting the order in the method name — there is no `u16`, only
//! [`Reader::u16_le`] and [`Reader::u16_be`], so a field's order is visible at the
//! call site instead of being inherited from whatever the last line did.
//!
//! RDP itself is little-endian. The exceptions are the layers it borrowed from the
//! ITU — the TPKT length, and the ASN.1 of T.125 MCS and T.124 GCC — which are
//! big-endian, so both orders appear within a few bytes of each other during the
//! connection sequence.

/// Why a PDU could not be read.
///
/// Both variants mean the same thing to a caller — this connection cannot continue —
/// so the value of separating them is in the sentence each one writes. A [`Short`]
/// names a buffer that ended early; a [`Refused`] names the field whose value stopped
/// us, which is the one a person needs when a particular server does not work.
///
/// [`Short`]: Malformed::Short
/// [`Refused`]: Malformed::Refused
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    /// The buffer ended in the middle of a field.
    #[error("{what} is {len} bytes, and reading {need} more at offset {at} runs past the end")]
    Short { what: &'static str, len: usize, at: usize, need: usize },
    /// A field carries a value this client will not go on from.
    #[error("{what} carries {field} {value:#x}, which this client does not accept")]
    Refused { what: &'static str, field: &'static str, value: u64 },
}

/// A buffer being taken apart, front to back.
///
/// `what` is the name of the structure being read — "an X.224 Connection Confirm" —
/// and every error the reader produces starts with it. Carrying it on the reader
/// rather than passing it to each call is what keeps a decoder readable: the field
/// reads stay one line each, and the message still says which PDU went wrong.
pub struct Reader<'a> {
    what: &'static str,
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(what: &'static str, bytes: &'a [u8]) -> Self {
        Self { what, bytes, at: 0 }
    }

    /// Where the next read starts.
    pub fn at(&self) -> usize {
        self.at
    }

    /// Whether everything has been read.
    pub fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    /// What has not been read yet, left where it is.
    pub fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at..]
    }

    /// The next `n` bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(
            Malformed::Short { what: self.what, len: self.bytes.len(), at: self.at, need: n },
        )?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    /// Step over `n` bytes, failing if they are not there.
    ///
    /// Skipping is not the same as ignoring: a field this client does not read still
    /// has to be present, or the fields after it are not where the decoder thinks.
    pub fn skip(&mut self, n: usize) -> Result<(), Malformed> {
        self.bytes(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16_le(&mut self) -> Result<u16, Malformed> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u16_be(&mut self) -> Result<u16, Malformed> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub fn u32_le(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u32_be(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    /// The error for a field whose value this client will not go on from.
    pub fn refuse(&self, field: &'static str, value: impl Into<u64>) -> Malformed {
        Malformed::Refused { what: self.what, field, value: value.into() }
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let bytes = self.bytes(N)?;
        Ok(bytes.try_into().expect("`bytes` yields exactly the length it was asked for"))
    }
}

/// A PDU being built, front to back.
///
/// Infallible, because a `Vec` does not run out: the length checks that belong to a
/// PDU — TPKT's 16-bit frame length, MCS's channel chunking — belong to the layer
/// that knows the limit, not to every field write.
#[derive(Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A writer for a PDU whose size is already known, so the buffer is allocated
    /// once.
    pub fn with_capacity(bytes: usize) -> Self {
        Self { bytes: Vec::with_capacity(bytes) }
    }

    pub fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn u16_le(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u16_be(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn u32_le(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u32_be(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    /// `count` zero bytes, for a field that is reserved or padded.
    pub fn zeros(&mut self, count: usize) {
        self.bytes.resize(self.bytes.len() + count, 0);
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_read_in_the_order_its_name_gives() {
        let mut r = Reader::new("a test", &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        assert_eq!(r.u16_le().unwrap(), 0x0201);
        assert_eq!(r.u16_be().unwrap(), 0x0304);
        assert_eq!(r.u8().unwrap(), 0x05);
        assert_eq!(r.rest(), &[0x06]);
        assert!(!r.is_empty());
        assert_eq!(r.u8().unwrap(), 0x06);
        assert!(r.is_empty());
    }

    /// The end of the buffer is a value, not a panic — these buffers come off a
    /// socket, and a truncated one is a connection to end rather than a crash.
    #[test]
    fn reading_past_the_end_names_the_structure_and_the_offset() {
        let mut r = Reader::new("an X.224 Connection Confirm", &[0x03, 0x00]);
        assert_eq!(r.u8().unwrap(), 3);
        let err = r.u32_le().unwrap_err();
        assert_eq!(
            err.to_string(),
            "an X.224 Connection Confirm is 2 bytes, and reading 4 more at offset 1 runs past the \
             end"
        );
        // And the reader did not move, so the offset in that message stays true.
        assert_eq!(r.at(), 1);

        // A length that would overflow the offset is short, not a wrapped success.
        assert!(matches!(r.bytes(usize::MAX).unwrap_err(), Malformed::Short { .. }));
    }

    #[test]
    fn a_refused_field_names_itself() {
        let r = Reader::new("a TPKT header", &[]);
        assert_eq!(
            r.refuse("its version", 5_u8).to_string(),
            "a TPKT header carries its version 0x5, which this client does not accept"
        );
    }

    #[test]
    fn a_writer_lays_the_bytes_down_in_order() {
        let mut w = Writer::new();
        w.u8(0xE0);
        w.u16_be(0x0102);
        w.u32_le(0x0000_0003);
        w.zeros(2);
        w.bytes(b"hi");
        assert_eq!(w.finish(), vec![0xE0, 0x01, 0x02, 0x03, 0, 0, 0, 0, 0, b'h', b'i']);
    }
}
