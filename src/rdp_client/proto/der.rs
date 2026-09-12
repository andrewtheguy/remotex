//! Reading ASN.1, the tag-length-value way.
//!
//! Two of the layers a connection has to get through are ASN.1 rather than RDP's own
//! packed structures: the X.509 certificate TLS hands back, and the T.125 MCS connect
//! PDUs that follow. Both are encoded in the same tag-length-value shape — DER for the
//! certificate, BER for MCS, which for what RDP sends differ only in whether a length
//! may be written longer than it needs to be.
//!
//! There is no schema here and no object identifiers: a codec walks the structure it
//! already knows, naming the tag it expects at each step, and anything else ends the
//! connection. Writing is the same walk in reverse — with the wrinkle that a length
//! comes before the bytes it measures, so a nested structure is built into its own
//! [`Writer`] and prefixed once its size is known.
//!
//! [`Writer`]: super::wire::Writer

use super::wire::{Malformed, Reader, Writer};

pub const BOOLEAN: u8 = 0x01;
pub const INTEGER: u8 = 0x02;
pub const BIT_STRING: u8 = 0x03;
pub const OCTET_STRING: u8 = 0x04;
pub const ENUMERATED: u8 = 0x0A;
pub const SEQUENCE: u8 = 0x30;

/// A context-specific constructed tag, `[n]`, which is how both X.509 and MCS mark
/// an optional field by position rather than by type.
pub const fn context(n: u8) -> u8 {
    0xA0 | n
}

/// The tag of the next value, without consuming it.
///
/// For the one thing a walker needs to decide before it reads: whether an optional
/// field is present.
pub fn peek(r: &Reader<'_>) -> Option<u8> {
    r.rest().first().copied()
}

/// The contents of the next value, which must carry `tag`.
pub fn expect<'a>(r: &mut Reader<'a>, tag: u8) -> Result<&'a [u8], Malformed> {
    let (found, length) = header(r)?;
    if found != tag {
        return Err(r.refuse("an ASN.1 tag", found));
    }
    r.bytes(length)
}

/// Step over the next value, whatever it is.
pub fn skip(r: &mut Reader<'_>) -> Result<(), Malformed> {
    let (_, length) = header(r)?;
    r.skip(length)
}

/// The tag and the length of the next value, leaving the reader at its contents.
pub fn header(r: &mut Reader<'_>) -> Result<(u8, usize), Malformed> {
    let tag = r.u8()?;
    Ok((tag, read_length(r)?))
}

/// The length of the value whose tag has just been read.
///
/// Only the definite forms: a short length is one byte under 128, a long one is a
/// count of length bytes followed by that many. The indefinite form — length `0x80`,
/// terminated by two zero bytes — is legal BER that neither X.509 nor anything RDP
/// sends uses, and accepting it would mean scanning for a terminator inside data this
/// module does not parse.
pub fn read_length(r: &mut Reader<'_>) -> Result<usize, Malformed> {
    let first = r.u8()?;
    if first < 0x80 {
        return Ok(usize::from(first));
    }
    let count = usize::from(first & 0x7F);
    // `0x80` is the indefinite form and `0xFF` is reserved; a length longer than a
    // pointer cannot describe a buffer that exists.
    if count == 0 || count > 4 {
        return Err(r.refuse("an ASN.1 length of", first));
    }
    let mut length = 0_usize;
    for byte in r.bytes(count)? {
        length = (length << 8) | usize::from(*byte);
    }
    Ok(length)
}

/// The public key out of a DER-encoded X.509 certificate: the `subjectPublicKey` bit
/// string of its `SubjectPublicKeyInfo`, contents only.
///
/// CredSSP binds the credential exchange to exactly these bytes — the server proves it
/// holds the private key of the certificate that terminated the TLS session, which is
/// what stops an interceptor from replaying the credentials on to the real host. Any
/// other slice of the certificate would authenticate nothing.
pub fn certificate_public_key(der: &[u8]) -> Result<Vec<u8>, Malformed> {
    const WHAT: &str = "a server certificate";
    let mut r = Reader::new(WHAT, der);

    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
    let mut tbs = Reader::new(WHAT, expect(&mut r, SEQUENCE)?);
    // TBSCertificate ::= SEQUENCE { [0] version DEFAULT v1, serialNumber, signature,
    //                               issuer, validity, subject, subjectPublicKeyInfo, ... }
    let mut fields = Reader::new(WHAT, expect(&mut tbs, SEQUENCE)?);
    if peek(&fields) == Some(context(0)) {
        skip(&mut fields)?;
    }
    for _ in 0..5 {
        skip(&mut fields)?;
    }

    // SubjectPublicKeyInfo ::= SEQUENCE { algorithm, subjectPublicKey BIT STRING }
    let mut spki = Reader::new(WHAT, expect(&mut fields, SEQUENCE)?);
    skip(&mut spki)?;
    let bits = expect(&mut spki, BIT_STRING)?;
    // The first byte of a bit string is how many bits of the last byte are padding.
    // A key is whole bytes, so it is zero, and the key is everything after it.
    let (unused, key) =
        bits.split_first().ok_or(Malformed::Short { what: WHAT, len: 0, at: 0, need: 1 })?;
    if *unused != 0 || key.is_empty() {
        return Err(Malformed::Refused {
            what: WHAT,
            field: "a public key with unused bits",
            value: u64::from(*unused),
        });
    }
    Ok(key.to_vec())
}

/// The contents of an `[APPLICATION n]` value, which is how MCS names its two connect
/// PDUs.
///
/// Only the long tag form, `0x7F` followed by the number: the numbers RDP uses — 101
/// and 102 — are past the 30 that fit the tag byte itself.
pub fn expect_application<'a>(r: &mut Reader<'a>, tag: u8) -> Result<&'a [u8], Malformed> {
    let first = r.u8()?;
    if first != APPLICATION_LONG {
        return Err(r.refuse("an ASN.1 tag", first));
    }
    let found = r.u8()?;
    if found != tag {
        return Err(r.refuse("an ASN.1 application tag", found));
    }
    let length = read_length(r)?;
    r.bytes(length)
}

/// An INTEGER, read as the unsigned number RDP always means by one.
pub fn read_integer(r: &mut Reader<'_>) -> Result<u32, Malformed> {
    let bytes = expect(r, INTEGER)?;
    // Four bytes plus a leading zero is the longest a 32-bit value can be written.
    if bytes.is_empty() || bytes.len() > 5 {
        return Err(r.refuse("an INTEGER of", u64::try_from(bytes.len()).unwrap_or(u64::MAX)));
    }
    let mut value = 0_u64;
    for byte in bytes {
        value = (value << 8) | u64::from(*byte);
    }
    u32::try_from(value).map_err(|_| r.refuse("an INTEGER", value))
}

/// An ENUMERATED, which is always one byte in what RDP sends.
pub fn read_enumerated(r: &mut Reader<'_>) -> Result<u8, Malformed> {
    let bytes = expect(r, ENUMERATED)?;
    match bytes {
        [value] => Ok(*value),
        other => Err(r.refuse("an ENUMERATED of", u64::try_from(other.len()).unwrap_or(u64::MAX))),
    }
}

/// The first byte of a long-form tag: application class, constructed, tag number to
/// follow.
const APPLICATION_LONG: u8 = 0x7F;

/// A definite length, in the shortest form that holds it.
pub fn write_length(w: &mut Writer, length: usize) {
    match u8::try_from(length) {
        Ok(byte) if byte <= 0x7F => w.u8(byte),
        Ok(byte) => {
            w.u8(0x81);
            w.u8(byte);
        }
        Err(_) => {
            w.u8(0x82);
            w.u16_be(u16::try_from(length).expect("a connect PDU fits the TPKT length before this"));
        }
    }
}

/// A tag and the length of the value that follows it.
pub fn write_tag(w: &mut Writer, tag: u8, length: usize) {
    w.u8(tag);
    write_length(w, length);
}

/// An `[APPLICATION n]` tag and the length of the value that follows it.
pub fn write_application_tag(w: &mut Writer, tag: u8, length: usize) {
    w.u8(APPLICATION_LONG);
    w.u8(tag);
    write_length(w, length);
}

pub fn write_octet_string(w: &mut Writer, bytes: &[u8]) {
    write_tag(w, OCTET_STRING, bytes.len());
    w.bytes(bytes);
}

pub fn write_boolean(w: &mut Writer, value: bool) {
    write_tag(w, BOOLEAN, 1);
    w.u8(if value { 0xFF } else { 0x00 });
}

/// An INTEGER: the fewest bytes that hold the value, big-endian, with a leading zero
/// when the top bit would otherwise make it negative.
pub fn write_integer(w: &mut Writer, value: u32) {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len() - 1);
    let digits = &bytes[first..];
    let pad = usize::from(digits[0] & 0x80 != 0);
    write_tag(w, INTEGER, digits.len() + pad);
    if pad == 1 {
        w.u8(0);
    }
    w.bytes(digits);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One tag-length-value, with the length in whichever form fits.
    fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let length = contents.len();
        if length < 0x80 {
            out.push(u8::try_from(length).unwrap());
        } else {
            let bytes = length.to_be_bytes();
            let bytes = &bytes[bytes.iter().position(|b| *b != 0).unwrap()..];
            out.push(0x80 | u8::try_from(bytes.len()).unwrap());
            out.extend_from_slice(bytes);
        }
        out.extend_from_slice(contents);
        out
    }

    #[test]
    fn a_length_is_read_in_both_forms() {
        let short = tlv(INTEGER, &[7]);
        assert_eq!(header(&mut Reader::new("a test", &short)).unwrap(), (INTEGER, 1));

        let long = tlv(OCTET_STRING, &vec![0; 300]);
        assert_eq!(header(&mut Reader::new("a test", &long)).unwrap(), (OCTET_STRING, 300));
        assert_eq!(expect(&mut Reader::new("a test", &long), OCTET_STRING).unwrap().len(), 300);
    }

    #[test]
    fn a_length_this_reader_will_not_walk_is_refused() {
        // The indefinite form, and a reserved one.
        for first in [0x80_u8, 0xFF] {
            let bytes = [SEQUENCE, first, 0, 0];
            assert!(matches!(
                header(&mut Reader::new("a test", &bytes)).unwrap_err(),
                Malformed::Refused { .. }
            ));
        }
    }

    #[test]
    fn an_unexpected_tag_ends_the_walk() {
        let bytes = tlv(INTEGER, &[7]);
        assert!(matches!(
            expect(&mut Reader::new("a test", &bytes), SEQUENCE).unwrap_err(),
            Malformed::Refused { field: "an ASN.1 tag", value: 2, .. }
        ));
    }

    fn written(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        f(&mut w);
        w.finish()
    }

    #[test]
    fn a_written_length_takes_the_shortest_form_and_reads_back() {
        assert_eq!(written(|w| write_length(w, 0x7F)), vec![0x7F]);
        assert_eq!(written(|w| write_length(w, 0x80)), vec![0x81, 0x80]);
        assert_eq!(written(|w| write_length(w, 0x1234)), vec![0x82, 0x12, 0x34]);
        for length in [0_usize, 1, 0x7F, 0x80, 0xFF, 0x100, 0xFFFF] {
            let bytes = written(|w| write_length(w, length));
            assert_eq!(read_length(&mut Reader::new("a test", &bytes)).unwrap(), length);
        }
    }

    /// The values MCS writes, and the one that needs a leading zero so a positive
    /// number is not read as a negative one.
    #[test]
    fn an_integer_is_written_as_the_positive_number_it_is() {
        assert_eq!(written(|w| write_integer(w, 0)), vec![INTEGER, 1, 0]);
        assert_eq!(written(|w| write_integer(w, 2)), vec![INTEGER, 1, 2]);
        assert_eq!(written(|w| write_integer(w, 0x80)), vec![INTEGER, 2, 0x00, 0x80]);
        assert_eq!(written(|w| write_integer(w, 0xFFFF)), vec![INTEGER, 3, 0x00, 0xFF, 0xFF]);
        assert_eq!(written(|w| write_integer(w, 0xFC17)), vec![INTEGER, 3, 0x00, 0xFC, 0x17]);

        for value in [0_u32, 1, 0x7F, 0x80, 0xFF, 0x420, 0xFC17, 0xFFFF, u32::MAX] {
            let bytes = written(|w| write_integer(w, value));
            assert_eq!(read_integer(&mut Reader::new("a test", &bytes)).unwrap(), value);
        }
    }

    #[test]
    fn an_application_tag_is_written_and_read_by_its_number() {
        let bytes = written(|w| {
            write_application_tag(w, 101, 2);
            w.bytes(b"hi");
        });
        assert_eq!(bytes, vec![0x7F, 101, 2, b'h', b'i']);
        assert_eq!(expect_application(&mut Reader::new("a test", &bytes), 101).unwrap(), b"hi");
        let err = expect_application(&mut Reader::new("a test", &bytes), 102).unwrap_err();
        assert!(matches!(err, Malformed::Refused { field: "an ASN.1 application tag", .. }));
    }

    #[test]
    fn a_boolean_and_an_octet_string_are_written_the_way_mcs_sends_them() {
        assert_eq!(written(|w| write_boolean(w, true)), vec![BOOLEAN, 1, 0xFF]);
        assert_eq!(written(|w| write_octet_string(w, &[1])), vec![OCTET_STRING, 1, 1]);
        assert_eq!(
            read_enumerated(&mut Reader::new("a test", &[ENUMERATED, 1, 0])).unwrap(),
            0
        );
    }

    /// A certificate shaped like the ones a Windows host sends: a version tag, five
    /// fields nobody here reads, then the key.
    fn certificate(version: bool, key: &[u8]) -> Vec<u8> {
        let mut fields = Vec::new();
        if version {
            fields.extend_from_slice(&tlv(context(0), &tlv(INTEGER, &[2])));
        }
        for _ in 0..5 {
            fields.extend_from_slice(&tlv(SEQUENCE, &[]));
        }
        let mut bits = vec![0];
        bits.extend_from_slice(key);
        let mut spki = tlv(SEQUENCE, &[]);
        spki.extend_from_slice(&tlv(BIT_STRING, &bits));
        fields.extend_from_slice(&tlv(SEQUENCE, &spki));

        let mut certificate = tlv(SEQUENCE, &fields);
        certificate.extend_from_slice(&tlv(SEQUENCE, &[]));
        tlv(SEQUENCE, &certificate)
    }

    #[test]
    fn a_certificate_yields_its_public_key() {
        let key = vec![0xAB; 270];
        assert_eq!(certificate_public_key(&certificate(true, &key)).unwrap(), key);
        // A v1 certificate has no version tag, and the walk must not eat a field.
        assert_eq!(certificate_public_key(&certificate(false, &key)).unwrap(), key);
    }

    #[test]
    fn a_certificate_that_is_not_one_is_an_error_rather_than_a_wrong_key() {
        assert!(certificate_public_key(&[]).is_err());
        assert!(certificate_public_key(&tlv(SEQUENCE, &[])).is_err());
        let truncated = certificate(true, &[1, 2, 3]);
        assert!(certificate_public_key(&truncated[..truncated.len() - 2]).is_err());
    }
}
