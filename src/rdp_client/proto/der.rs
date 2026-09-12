//! Reading ASN.1, the tag-length-value way.
//!
//! Two of the layers a connection has to get through are ASN.1 rather than RDP's own
//! packed structures: the X.509 certificate TLS hands back, and the T.125 MCS connect
//! PDUs that follow. Both are encoded in the same tag-length-value shape — DER for the
//! certificate, BER for MCS, which for what RDP sends differ only in whether a length
//! may be written longer than it needs to be.
//!
//! This is a reader for that shape and nothing more. There is no schema here and no
//! object identifiers: a decoder walks the structure it already knows, naming the tag
//! it expects at each step, and anything else ends the connection.

use super::wire::{Malformed, Reader};

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
///
/// Only the definite forms: a short length is one byte under 128, a long one is a
/// count of length bytes followed by that many. The indefinite form — length `0x80`,
/// terminated by two zero bytes — is legal BER that neither X.509 nor anything RDP
/// sends uses, and accepting it would mean scanning for a terminator inside data this
/// module does not parse.
pub fn header(r: &mut Reader<'_>) -> Result<(u8, usize), Malformed> {
    let tag = r.u8()?;
    let first = r.u8()?;
    if first < 0x80 {
        return Ok((tag, usize::from(first)));
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
    Ok((tag, length))
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
