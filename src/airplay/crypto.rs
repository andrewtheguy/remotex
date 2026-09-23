//! The cryptography an AirPlay 1 receiver is asked for: the RSA key every such
//! receiver shares, the per-packet AES, and the password's RTSP Digest.
//!
//! The key is the one extracted from the AirPort Express, which shairport-sync
//! ships in `common.c`. It proves nothing about the receiver: a sender checks
//! only that whatever answers can use it. Kept as its two primes, which is all
//! `rsa` needs to rebuild it — this crate builds `rsa` without its PEM parser.

use std::net::IpAddr;
use std::sync::LazyLock;

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt as _, KeyInit as _};
use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use md5::{Digest as _, Md5};
use rsa::{BoxedUint, Oaep, Pkcs1v15Sign, RsaPrivateKey};

/// Senders drop base64 padding in both directions, so decoding accepts either
/// and encoding leaves it off, as the AirPort Express did.
pub const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The realm a password challenge names; shairport-sync's, and the one senders expect.
pub const REALM: &str = "raop";

const P: [u8; 128] = [
    0xf7, 0xe0, 0xbf, 0x5a, 0x1e, 0x67, 0x18, 0x31, 0x9a, 0x8b, 0x62, 0x09,
    0xc3, 0x17, 0x14, 0x44, 0x04, 0x59, 0xf9, 0x73, 0x85, 0x66, 0x13, 0xb1,
    0x7a, 0xe1, 0x50, 0x8b, 0xb3, 0xe6, 0x31, 0x6e, 0x6b, 0x7f, 0x46, 0x2d,
    0x2f, 0x7d, 0x64, 0x41, 0x2b, 0x84, 0xb7, 0x6b, 0xc2, 0x3f, 0x2b, 0x0c,
    0x35, 0x62, 0x45, 0x52, 0x79, 0xb2, 0x43, 0xa9, 0xf7, 0x31, 0x6f, 0x95,
    0x80, 0x07, 0xb3, 0x4c, 0x61, 0xf7, 0x68, 0xe2, 0xd4, 0x4e, 0xd5, 0xff,
    0x2b, 0x27, 0x28, 0x17, 0xec, 0x32, 0xb3, 0xe4, 0x93, 0x92, 0x92, 0x28,
    0xfa, 0xe7, 0x8e, 0x77, 0x4c, 0xa0, 0xf7, 0x5e, 0xbd, 0x69, 0xd5, 0x92,
    0x02, 0x79, 0x8f, 0x11, 0x6e, 0x36, 0x0c, 0x64, 0x38, 0xb3, 0x2e, 0x1b,
    0xd8, 0xb9, 0xdc, 0x1e, 0x32, 0x32, 0xf0, 0xd3, 0x09, 0x18, 0x88, 0x3c,
    0xc4, 0x3e, 0xf8, 0xdd, 0xa2, 0x2c, 0x36, 0x91,
];

const Q: [u8; 128] = [
    0xef, 0x6f, 0xff, 0xf9, 0x94, 0xf1, 0xe5, 0x64, 0x41, 0xaa, 0x00, 0x35,
    0xfd, 0x19, 0xa0, 0xc8, 0xd6, 0xf0, 0x23, 0x78, 0xc7, 0x05, 0x80, 0xd9,
    0xc4, 0x84, 0x20, 0x79, 0x1d, 0xf4, 0x07, 0xc5, 0x91, 0xfb, 0x6e, 0xbf,
    0xca, 0x32, 0x2c, 0x30, 0x86, 0xdd, 0x90, 0x1f, 0xd2, 0xfa, 0xe1, 0xae,
    0xbb, 0x64, 0xad, 0xf6, 0xbb, 0x79, 0xff, 0x80, 0x51, 0xbe, 0xbd, 0x0c,
    0xd8, 0x20, 0xab, 0x89, 0x87, 0x40, 0x06, 0x01, 0xa7, 0xb2, 0xfe, 0x93,
    0x90, 0xca, 0xcc, 0x9a, 0xca, 0xb8, 0xed, 0x2b, 0xf9, 0x1d, 0x18, 0x6d,
    0x8f, 0x69, 0x64, 0x3d, 0x7e, 0xfe, 0x0f, 0x5d, 0x56, 0xdf, 0x75, 0x77,
    0xa2, 0xd0, 0x35, 0xea, 0x54, 0x13, 0xfc, 0x98, 0xd8, 0xf3, 0xf9, 0x08,
    0xda, 0x05, 0x9a, 0x37, 0x9d, 0xa4, 0xb1, 0xcc, 0x38, 0xf1, 0x5d, 0x56,
    0x0a, 0x83, 0xcc, 0x31, 0x71, 0x53, 0xc8, 0x4b,
];

static KEY: LazyLock<RsaPrivateKey> = LazyLock::new(|| {
    RsaPrivateKey::from_p_q(
        BoxedUint::from_be_slice_vartime(&P),
        BoxedUint::from_be_slice_vartime(&Q),
        BoxedUint::from(65_537u64),
    )
    .expect("the AirPort Express primes make a key")
});

/// The key's public half, for the tests that play the sender.
#[cfg(test)]
pub(super) fn public_key() -> rsa::RsaPublicKey {
    KEY.to_public_key()
}

/// The `Apple-Response` to an `Apple-Challenge`: the challenge, the address the
/// sender reached this receiver on and the hardware address it advertises, zero
/// padded to 32 bytes and signed with PKCS#1 v1.5 and no digest prefix.
pub fn apple_response(challenge_b64: &str, local: IpAddr, hw_addr: [u8; 6]) -> anyhow::Result<String> {
    let mut data = B64
        .decode(challenge_b64.trim())
        .context("the Apple-Challenge is not base64")?;
    if data.len() > 16 {
        bail!("the Apple-Challenge is {} bytes, not at most 16", data.len());
    }
    match local.to_canonical() {
        IpAddr::V4(v4) => data.extend_from_slice(&v4.octets()),
        IpAddr::V6(v6) => data.extend_from_slice(&v6.octets()),
    }
    data.extend_from_slice(&hw_addr);
    if data.len() < 32 {
        data.resize(32, 0);
    }
    let signature = KEY
        .sign(Pkcs1v15Sign::new_unprefixed(), &data)
        .context("signing the Apple-Challenge")?;
    Ok(B64.encode(signature))
}

/// The session's AES key, from the SDP's `rsaaeskey`: RSA-OAEP with SHA-1.
pub fn unwrap_aes_key(rsaaeskey_b64: &str) -> anyhow::Result<[u8; 16]> {
    let wrapped = B64
        .decode(rsaaeskey_b64.trim())
        .context("the rsaaeskey is not base64")?;
    let key = KEY
        .decrypt(Oaep::<sha1::Sha1>::new(), &wrapped)
        .context("unwrapping the rsaaeskey")?;
    let len = key.len();
    key.try_into()
        .map_err(|_| anyhow::anyhow!("the rsaaeskey unwrapped to {len} bytes, not 16"))
}

/// One RTP payload's AES-128-CBC, undone in place. Every packet starts the chain
/// again from the session IV, and the `len % 16` bytes after the last whole block
/// travel in the clear.
pub fn decrypt_payload(cipher: &Aes128, iv: &[u8; 16], payload: &mut [u8]) {
    let mut chain = *iv;
    for block in payload.as_chunks_mut::<16>().0 {
        let ciphertext = *block;
        cipher.decrypt_block(block.into());
        for (b, c) in block.iter_mut().zip(chain) {
            *b ^= c;
        }
        chain = ciphertext;
    }
}

/// The cipher a session's packets are decrypted with.
pub fn payload_cipher(key: &[u8; 16]) -> Aes128 {
    Aes128::new(key.into())
}

/// Whether an `Authorization` header answers the challenge `nonce` for `method`
/// with `password`: RFC 2617 Digest without `qop`, which is what AirPlay 1
/// senders compute, `MD5(MD5(user:realm:password):nonce:MD5(method:uri))` in hex.
pub fn digest_matches(authorization: &str, method: &str, nonce: &str, password: &str) -> bool {
    let Some(fields) = authorization.trim().strip_prefix("Digest ") else {
        return false;
    };
    let field = |name: &str| {
        fields.split(',').find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key.trim() == name).then(|| value.trim().trim_matches('"'))
        })
    };
    let (Some(username), Some(realm), Some(uri), Some(response)) =
        (field("username"), field("realm"), field("uri"), field("response"))
    else {
        return false;
    };
    // The nonce must be the one this connection issued; a sender replaying an
    // answer to someone else's challenge is not answering this one.
    if field("nonce").is_some_and(|echoed| echoed != nonce) {
        return false;
    }
    let ha1 = md5_hex(&format!("{username}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    response.eq_ignore_ascii_case(&md5_hex(&format!("{ha1}:{nonce}:{ha2}")))
}

fn md5_hex(text: &str) -> String {
    Md5::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use aes::cipher::BlockCipherEncrypt as _;
    use rsa::RsaPublicKey;

    use super::*;

    /// The primes rebuild the AirPort Express key: its modulus begins `e7d744f2`
    /// and is 2048 bits, as the PEM shairport-sync ships says.
    #[test]
    fn the_primes_rebuild_the_airport_express_key() {
        use rsa::traits::PublicKeyParts as _;
        let n = KEY.n().to_be_bytes();
        let n = &n[n.iter().position(|&b| b != 0).unwrap()..];
        assert_eq!(n.len(), 256);
        assert_eq!(&n[..4], [0xe7, 0xd7, 0x44, 0xf2]);
    }

    #[test]
    fn a_wrapped_key_round_trips() {
        let public = RsaPublicKey::from(&*KEY);
        let aes = *b"0123456789abcdef";
        let wrapped = public
            .encrypt(&mut rand::rng(), Oaep::<sha1::Sha1>::new(), &aes)
            .unwrap();
        assert_eq!(unwrap_aes_key(&B64.encode(wrapped)).unwrap(), aes);
    }

    #[test]
    fn the_challenge_response_verifies_against_the_public_key() {
        let public = RsaPublicKey::from(&*KEY);
        let challenge = [7u8; 16];
        let hw = [0x02, 0x52, 0x58, 0, 0, 1];
        // A dual-stack socket reports an IPv4 sender as mapped; the four bytes signed
        // are the IPv4 address either way.
        let local: IpAddr = "::ffff:10.22.34.32".parse().unwrap();
        let response = apple_response(&B64.encode(challenge), local, hw).unwrap();
        let mut signed = challenge.to_vec();
        signed.extend_from_slice(&[10, 22, 34, 32]);
        signed.extend_from_slice(&hw);
        signed.resize(32, 0);
        public
            .verify(Pkcs1v15Sign::new_unprefixed(), &signed, &B64.decode(response).unwrap())
            .unwrap();
    }

    /// Whole blocks are chained from the IV again on every packet; the tail is clear.
    #[test]
    fn a_payload_decrypts_block_by_block_and_leaves_its_tail() {
        let key = *b"remotex-airplay!";
        let iv = *b"0123456789abcdef";
        let plain: Vec<u8> = (0..37).collect();
        let cipher = payload_cipher(&key);
        let mut sealed = plain.clone();
        let mut chain = iv;
        for block in sealed.as_chunks_mut::<16>().0 {
            for (b, c) in block.iter_mut().zip(chain) {
                *b ^= c;
            }
            cipher.encrypt_block(block.into());
            chain = *block;
        }
        assert_eq!(sealed[32..], plain[32..], "the tail is sent as it is");
        decrypt_payload(&cipher, &iv, &mut sealed);
        assert_eq!(sealed, plain);
    }

    /// RFC 2617's own example, without `qop`, the way a sender answers.
    #[test]
    fn a_digest_is_checked_against_the_password_and_the_nonce() {
        let nonce = "dcd98b7102dd2f0e8b11d0f600bfb0c093";
        let ha1 = md5_hex("iTunes:raop:secret");
        let ha2 = md5_hex("ANNOUNCE:rtsp://10.0.0.1/123");
        let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
        let header = format!(
            r#"Digest username="iTunes", realm="raop", nonce="{nonce}", uri="rtsp://10.0.0.1/123", response="{}""#,
            response.to_uppercase()
        );
        assert!(digest_matches(&header, "ANNOUNCE", nonce, "secret"));
        assert!(!digest_matches(&header, "ANNOUNCE", nonce, "wrong"));
        assert!(!digest_matches(&header, "SETUP", nonce, "secret"), "the method is covered");
        assert!(!digest_matches(&header, "ANNOUNCE", "another-nonce", "secret"));
        assert!(!digest_matches("Basic abc", "ANNOUNCE", nonce, "secret"));
    }
}
