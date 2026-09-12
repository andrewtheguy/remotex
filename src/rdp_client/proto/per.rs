//! Packed Encoding Rules, the aligned variant, as T.124 uses them.
//!
//! The GCC conference the RDP connection sequence carries is ASN.1 — but not the
//! tag-length-value ASN.1 of [`super::der`]. PER drops the tags: the decoder knows the
//! schema, so a field is just its value, and only the things whose size is not fixed by
//! the schema — lengths, counts, unbounded integers — carry any framing at all.
//!
//! That makes PER unreadable without the schema beside it, which is why every caller
//! here writes the field's ASN.1 name in a comment next to the line that encodes it.
//! It also makes it small: the whole of PER that RDP uses is the handful of functions
//! below, plus single bytes written directly for a CHOICE index, an OPTIONAL bitmap,
//! a SET OF count and an ENUMERATED value, each of which is one byte and nothing more.
//!
//! Everything here is big-endian, unlike RDP's own structures.

use super::wire::{Malformed, Reader, Writer};

/// The largest length the two-byte form can carry here — fifteen bits, not the
/// fourteen PER specifies. See [`read_length`].
pub const MAX_LENGTH: u16 = 0x7FFF;

/// How many bytes [`write_length`] will use.
///
/// Needed because an enclosing length has to be written before the thing it measures,
/// and that measurement includes the inner length field itself.
pub fn length_size(length: u16) -> usize {
    if length > 0x7F { 2 } else { 1 }
}

/// A length: one byte under 128, otherwise two with the top bit set. See
/// [`read_length`] for why fifteen bits and not fourteen.
pub fn write_length(w: &mut Writer, length: u16) {
    assert!(length <= MAX_LENGTH, "a PER length is fifteen bits");
    if length > 0x7F {
        w.u16_be(length | 0x8000);
    } else {
        w.u8(u8::try_from(length).expect("a length of 0x7F or less fits a byte"));
    }
}

/// A length, in the fifteen-bit form RDP actually uses.
///
/// PER says the two-byte form is `10xxxxxx xxxxxxxx`, fourteen bits, and that anything
/// larger is sent as a chain of 16K fragments. RDP's MCS does not fragment: it takes
/// the whole low fifteen bits as the length, so a payload between 16K and 32K is one
/// two-byte length whose second-highest bit is set rather than two fragments. Reading
/// that bit as PER would mean rejecting PDUs every server sends, so it is read as part
/// of the length — which is also what leaves no encoding free to mean "fragmented",
/// and why nothing here can emit one.
pub fn read_length(r: &mut Reader<'_>) -> Result<u16, Malformed> {
    let first = r.u8()?;
    if first & 0x80 == 0 {
        return Ok(u16::from(first));
    }
    Ok((u16::from(first & 0x7F) << 8) | u16::from(r.u8()?))
}

/// An unconstrained INTEGER: a length, then that many bytes of value.
pub fn write_integer(w: &mut Writer, value: u32) {
    if let Ok(value) = u8::try_from(value) {
        write_length(w, 1);
        w.u8(value);
    } else if let Ok(value) = u16::try_from(value) {
        write_length(w, 2);
        w.u16_be(value);
    } else {
        write_length(w, 4);
        w.u32_be(value);
    }
}

pub fn read_integer(r: &mut Reader<'_>) -> Result<u32, Malformed> {
    let length = read_length(r)?;
    match length {
        0 => Ok(0),
        1 => Ok(u32::from(r.u8()?)),
        2 => Ok(u32::from(r.u16_be()?)),
        4 => Ok(r.u32_be()?),
        other => Err(r.refuse("a PER integer of", other)),
    }
}

/// An INTEGER with a lower bound, which PER stores as the distance from that bound.
///
/// Every user and channel identifier in MCS is one of these, with `min` 1001 for a
/// user and 0 for a channel — so the same number on the wire means different things in
/// the two fields, and the bound is not optional context.
pub fn write_integer16(w: &mut Writer, value: u16, min: u16) {
    w.u16_be(value.checked_sub(min).expect("an identifier is never below its own bound"));
}

pub fn read_integer16(r: &mut Reader<'_>, min: u16) -> Result<u16, Malformed> {
    let value = r.u16_be()?;
    value.checked_add(min).ok_or_else(|| r.refuse("an identifier of", value))
}

/// An OCTET STRING whose schema gives it a lower size bound: the length written is the
/// distance from that bound, so a fixed-size field — `min` equal to the length — writes
/// a zero length and then the bytes.
pub fn write_octet_string(w: &mut Writer, bytes: &[u8], min: usize) {
    let length = u16::try_from(bytes.len().saturating_sub(min))
        .expect("an octet string in the connection sequence is well under 32K");
    write_length(w, length);
    w.bytes(bytes);
}

pub fn read_octet_string<'a>(r: &mut Reader<'a>, min: usize) -> Result<&'a [u8], Malformed> {
    let length = usize::from(read_length(r)?);
    r.bytes(length + min)
}

/// The whole of an OCTET STRING this client knows the value of in advance — the H.221
/// key that says which of the GCC user data blocks are RDP's.
pub fn expect_octet_string(
    r: &mut Reader<'_>,
    field: &'static str,
    expected: &[u8],
) -> Result<(), Malformed> {
    let found = read_octet_string(r, expected.len())?;
    if found != expected {
        // The first byte is enough to name what arrived; the whole string would not
        // fit the error's shape and does not say more.
        return Err(r.refuse(field, found.first().copied().unwrap_or(0)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        f(&mut w);
        w.finish()
    }

    #[test]
    fn a_length_changes_form_at_128() {
        assert_eq!(written(|w| write_length(w, 0x7F)), vec![0x7F]);
        assert_eq!(written(|w| write_length(w, 0x80)), vec![0x80, 0x80]);
        assert_eq!(written(|w| write_length(w, MAX_LENGTH)), vec![0xFF, 0xFF]);
        assert_eq!(length_size(0x7F), 1);
        assert_eq!(length_size(0x80), 2);

        for length in [0_u16, 1, 0x7F, 0x80, 0x1234, MAX_LENGTH] {
            let bytes = written(|w| write_length(w, length));
            assert_eq!(bytes.len(), length_size(length));
            assert_eq!(read_length(&mut Reader::new("a test", &bytes)).unwrap(), length);
        }
    }

    /// The bit PER reserves for fragmentation, which RDP spends on length.
    #[test]
    fn the_two_byte_form_carries_fifteen_bits() {
        assert_eq!(read_length(&mut Reader::new("a test", &[0xC1, 0x23])).unwrap(), 0x4123);
        assert_eq!(written(|w| write_length(w, 0x4123)), vec![0xC1, 0x23]);
    }

    #[test]
    fn an_integer_takes_the_fewest_bytes_that_hold_it() {
        assert_eq!(written(|w| write_integer(w, 0)), vec![1, 0]);
        assert_eq!(written(|w| write_integer(w, 0xFF)), vec![1, 0xFF]);
        assert_eq!(written(|w| write_integer(w, 0x0100)), vec![2, 0x01, 0x00]);
        assert_eq!(written(|w| write_integer(w, 0x0001_0000)), vec![4, 0, 1, 0, 0]);

        for value in [0_u32, 1, 0xFF, 0x100, 0xFFFF, 0x1_0000, u32::MAX] {
            let bytes = written(|w| write_integer(w, value));
            assert_eq!(read_integer(&mut Reader::new("a test", &bytes)).unwrap(), value);
        }
    }

    /// The bound is part of the field, not decoration: 1001 on the wire is user 2002
    /// and channel 1001.
    #[test]
    fn a_bounded_integer_is_the_distance_from_its_bound() {
        assert_eq!(written(|w| write_integer16(w, 1004, 1001)), vec![0x00, 0x03]);
        assert_eq!(written(|w| write_integer16(w, 1004, 0)), vec![0x03, 0xEC]);
        assert_eq!(read_integer16(&mut Reader::new("a test", &[0x00, 0x03]), 1001).unwrap(), 1004);
        assert_eq!(read_integer16(&mut Reader::new("a test", &[0x00, 0x03]), 0).unwrap(), 3);
        assert!(read_integer16(&mut Reader::new("a test", &[0xFF, 0xFF]), 1001).is_err());
    }

    #[test]
    fn a_fixed_size_octet_string_writes_no_length_of_its_own() {
        assert_eq!(written(|w| write_octet_string(w, b"Duca", 4)), b"\x00Duca".to_vec());
        assert_eq!(written(|w| write_octet_string(w, b"ab", 0)), b"\x02ab".to_vec());

        let mut r = Reader::new("a test", b"\x00McDn");
        expect_octet_string(&mut r, "an H.221 key", b"McDn").unwrap();
        assert!(r.is_empty());

        let mut r = Reader::new("a test", b"\x00Duca");
        let err = expect_octet_string(&mut r, "an H.221 key", b"McDn").unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "an H.221 key", .. }));
    }
}
