//! The pre-shared key: one 49-character string that both the gateway target
//! profile and the agent config file carry verbatim.
//!
//! Format (the house format also used by flextunnel and ezvpn):
//!
//! ```text
//! rxa<base64url-no-pad( 32 random bytes ‖ CRC16-CCITT-FALSE(those bytes), BE )>
//! ```
//!
//! The CRC exists so a mistyped key is rejected with "checksum mismatch"
//! instead of an opaque Noise handshake failure ten seconds later — the two are
//! very different debugging experiences. It is a typo check and nothing more:
//! the key's security comes entirely from the 32 random bytes.

use base64::Engine as _;

/// The literal prefix every PSK starts with.
pub const PREFIX: &str = "rxa";

/// Length of the key material the Noise handshake consumes.
pub const KEY_LEN: usize = 32;

/// Total length of the textual form: 3 prefix characters + 46 base64url
/// characters for the 34 encoded bytes (32 key + 2 CRC).
pub const TEXT_LEN: usize = 49;

/// Why a PSK string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PskError {
    #[error("PSK must start with `{PREFIX}`")]
    BadPrefix,
    #[error("PSK must be {TEXT_LEN} characters, got {0}")]
    BadLength(usize),
    #[error("PSK is not valid base64url")]
    BadEncoding,
    #[error("PSK checksum mismatch — check for a transcription typo")]
    BadChecksum,
}

/// Mint a fresh PSK. Printed by `remotex gen-psk` and pasted into both config
/// files.
pub fn generate() -> String {
    let mut key = [0u8; KEY_LEN];
    // The OS CSPRNG. A failure here means the system has no entropy source at
    // all, which is not a condition this program can sensibly continue past.
    getrandom::fill(&mut key).expect("the OS random number generator is unavailable");
    encode(&key)
}

/// Render raw key material in the textual form. Split out from [`generate`] so
/// the roundtrip test can drive it with known bytes.
fn encode(key: &[u8; KEY_LEN]) -> String {
    let mut bytes = Vec::with_capacity(KEY_LEN + 2);
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(&crc16_ccitt_false(key).to_be_bytes());
    format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
    )
}

/// Parse the textual form back into key material, verifying the checksum.
pub fn parse(text: &str) -> Result<[u8; KEY_LEN], PskError> {
    let text = text.trim();
    let body = text.strip_prefix(PREFIX).ok_or(PskError::BadPrefix)?;
    if text.len() != TEXT_LEN {
        return Err(PskError::BadLength(text.len()));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| PskError::BadEncoding)?;
    // Guaranteed by the length check above, but the slicing below relies on it.
    if bytes.len() != KEY_LEN + 2 {
        return Err(PskError::BadEncoding);
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes[..KEY_LEN]);
    let want = u16::from_be_bytes([bytes[KEY_LEN], bytes[KEY_LEN + 1]]);
    if crc16_ccitt_false(&key) != want {
        return Err(PskError::BadChecksum);
    }
    Ok(key)
}

/// CRC16/CCITT-FALSE: polynomial 0x1021, init 0xFFFF, no reflection, no final
/// XOR. Small enough not to be worth a dependency.
fn crc16_ccitt_false(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    // The published CRC16/CCITT-FALSE check value: "123456789" -> 0x29B1.
    #[test]
    fn crc_matches_the_reference_check_value() {
        assert_eq!(crc16_ccitt_false(b"123456789"), 0x29B1);
        assert_eq!(crc16_ccitt_false(b""), 0xFFFF);
    }

    #[test]
    fn generated_psk_has_the_documented_shape() {
        let psk = generate();
        assert_eq!(psk.len(), TEXT_LEN, "{psk}");
        assert!(psk.starts_with(PREFIX), "{psk}");
        // base64url: no '+', '/' or '=' that would need shell quoting.
        assert!(
            psk[PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{psk}"
        );
    }

    #[test]
    fn generate_parse_roundtrips_and_is_not_constant() {
        let a = generate();
        let b = generate();
        assert_ne!(a, b, "two generated PSKs must not collide");
        assert_ne!(parse(&a).unwrap(), parse(&b).unwrap());
        assert_eq!(parse(&encode(&[7u8; KEY_LEN])).unwrap(), [7u8; KEY_LEN]);
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let psk = generate();
        assert_eq!(parse(&format!("  {psk}\n")).unwrap(), parse(&psk).unwrap());
    }

    #[test]
    fn bad_prefix_is_rejected() {
        let psk = generate();
        let wrong = format!("rxb{}", &psk[PREFIX.len()..]);
        assert_eq!(parse(&wrong), Err(PskError::BadPrefix));
        assert_eq!(parse(""), Err(PskError::BadPrefix));
    }

    #[test]
    fn bad_length_is_rejected() {
        let psk = generate();
        assert_eq!(
            parse(&psk[..TEXT_LEN - 1]),
            Err(PskError::BadLength(TEXT_LEN - 1))
        );
        assert_eq!(
            parse(&format!("{psk}A")),
            Err(PskError::BadLength(TEXT_LEN + 1))
        );
    }

    #[test]
    fn bad_base64_is_rejected() {
        // '*' is outside the base64url alphabet; length stays correct.
        let psk = generate();
        let mangled = format!("{}*{}", &psk[..TEXT_LEN - 1], "");
        assert_eq!(mangled.len(), TEXT_LEN);
        assert_eq!(parse(&mangled), Err(PskError::BadEncoding));
    }

    // The point of the CRC: a single transposed character is caught here
    // rather than surfacing as a handshake failure against the agent.
    #[test]
    fn a_single_character_typo_is_caught_by_the_checksum() {
        let psk = generate();
        let mut chars: Vec<char> = psk.chars().collect();
        // Flip one key byte's worth of characters, well before the CRC tail.
        chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        assert_eq!(parse(&typo), Err(PskError::BadChecksum));
    }
}
