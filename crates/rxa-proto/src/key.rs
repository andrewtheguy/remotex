//! The identity keys: an X25519 keypair per machine, each side pinning the
//! other's public half.
//!
//! Format (the house format also used by flextunnel and ezvpn):
//!
//! ```text
//! <prefix><base64url-no-pad( 32 key bytes ‖ CRC16-CCITT-FALSE(those bytes), BE )>
//! ```
//!
//! The CRC exists so a mistyped key is rejected with "checksum mismatch"
//! instead of an opaque Noise handshake failure ten seconds later — the two are
//! very different debugging experiences. It is a typo check and nothing more:
//! the key's security comes entirely from the 32 random bytes.
//!
//! ## The prefix carries the role, not just the kind
//!
//! There are four prefixes, not two, because there are four fields and each
//! takes exactly one kind of key:
//!
//! | | private | public |
//! |---|---|---|
//! | gateway | `rxgs` — `[rxa].private_key` | `rxgp` — the agent's `gateway_public_key` |
//! | agent | `rxas` — the agent's `private_key` | `rxap` — a target's `agent_public_key` |
//!
//! A gateway never dials a gateway and an agent never dials an agent, so a key
//! pasted into the wrong field is always a mistake and never a configuration.
//! It matters most at the one moment both public keys are in play — pairing two
//! machines — where a single shared prefix would make them look interchangeable
//! and a swap would surface as a handshake rejection with nothing to say why.
//! Here it is a parse error that names what was pasted.

use base64::Engine as _;

/// Which end of the protocol a key belongs to.
///
/// Each side uses its own role for its identity and the other's for its peer:
/// the gateway mints [`Role::Gateway`] and pins [`Role::Agent`], the agent the
/// reverse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The remotex server, which dials.
    Gateway,
    /// The Mac agent, which listens.
    Agent,
}

impl Role {
    /// The prefix on this role's private key.
    pub fn private_prefix(self) -> &'static str {
        match self {
            Role::Gateway => "rxgs",
            Role::Agent => "rxas",
        }
    }

    /// The prefix on this role's public key.
    pub fn public_prefix(self) -> &'static str {
        match self {
            Role::Gateway => "rxgp",
            Role::Agent => "rxap",
        }
    }
}

/// Every prefix, with what it labels — the table [`describe`] reads to name a
/// key that turned up in the wrong field.
const PREFIXES: [(&str, &str); 4] = [
    ("rxgs", "a gateway private key"),
    ("rxgp", "a gateway public key"),
    ("rxas", "an agent private key"),
    ("rxap", "an agent public key"),
];

/// Length of every prefix. Uniform so [`TEXT_LEN`] is one number.
pub const PREFIX_LEN: usize = 4;

/// Length of the key material the Noise handshake consumes.
pub const KEY_LEN: usize = 32;

/// Total length of the textual form: 4 prefix characters + 46 base64url
/// characters for the 34 encoded bytes (32 key + 2 CRC).
pub const TEXT_LEN: usize = PREFIX_LEN + 46;

/// Why a key string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The prefix is not the one this field takes. `found` names what *was*
    /// pasted when its prefix is one of the other three, which is the whole
    /// reason the roles are in the prefix — at the moment this fires, "that is
    /// a gateway public key" is worth far more than "bad prefix".
    #[error("{} must start with `{}`{}", .expected_what, .expected, found_suffix(.found))]
    BadPrefix {
        /// What the field wanted, e.g. "an agent public key".
        expected_what: &'static str,
        /// The prefix the field wanted.
        expected: &'static str,
        /// What was pasted instead, when it is a key of another kind.
        found: Option<&'static str>,
    },
    #[error("key must be {TEXT_LEN} characters, got {0}")]
    BadLength(usize),
    #[error("key is not valid base64url")]
    BadEncoding,
    #[error("key checksum mismatch — check for a transcription typo")]
    BadChecksum,
}

/// The tail of a [`KeyError::BadPrefix`] message: what was pasted, when it is
/// recognisably a key of another kind. A free function because the alternative
/// is a conditional inside a `#[error]` format string.
fn found_suffix(found: &Option<&'static str>) -> String {
    match found {
        Some(found) => format!(" — this is {found}"),
        None => String::new(),
    }
}

/// Mint a fresh private key for `role`.
///
/// The public half is [`public_of`] and is derived on demand rather than
/// stored: two copies of one fact in a config file is one of them going stale.
pub fn generate_private(role: Role) -> String {
    let mut key = [0u8; KEY_LEN];
    // The OS CSPRNG. A failure here means the system has no entropy source at
    // all, which is not a condition this program can sensibly continue past.
    getrandom::fill(&mut key).expect("the OS random number generator is unavailable");
    encode(role.private_prefix(), &key)
}

/// The public key matching a private one.
///
/// Curve25519 scalars are clamped where they are used rather than where they
/// are stored — snow does it in both `derive_pubkey` and `dh` — so the raw
/// bytes [`generate_private`] produces need no preparation here. This is the
/// same path snow's own `Builder::generate_keypair` takes, which the tests
/// check against.
pub fn public_of(private: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    use snow::params::DHChoice;
    use snow::resolvers::{CryptoResolver as _, DefaultResolver};

    // `Dh`'s methods need no import: the resolver hands back a `dyn Dh`, and
    // the trait object *is* the type they are on.
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .expect("snow's default resolver provides Curve25519");
    // `set` derives the public key as a side effect.
    dh.set(private);
    let mut public = [0u8; KEY_LEN];
    public.copy_from_slice(dh.pubkey());
    public
}

/// The textual public key matching a textual private one, for the two commands
/// whose whole job is to print it (`remotex rxa-pubkey`, `remotex-agent
/// --public-key`) and for the agent's settings dialog.
pub fn public_text_of(role: Role, private_text: &str) -> Result<String, KeyError> {
    let private = parse_private(role, private_text)?;
    Ok(encode(role.public_prefix(), &public_of(&private)))
}

/// Parse `role`'s private key.
pub fn parse_private(role: Role, text: &str) -> Result<[u8; KEY_LEN], KeyError> {
    parse_with(role.private_prefix(), private_what(role), text)
}

/// Parse `role`'s public key.
pub fn parse_public(role: Role, text: &str) -> Result<[u8; KEY_LEN], KeyError> {
    parse_with(role.public_prefix(), public_what(role), text)
}

/// "a gateway private key" / "an agent private key", for error messages.
fn private_what(role: Role) -> &'static str {
    match role {
        Role::Gateway => "a gateway private key",
        Role::Agent => "an agent private key",
    }
}

/// "a gateway public key" / "an agent public key", for error messages.
fn public_what(role: Role) -> &'static str {
    match role {
        Role::Gateway => "a gateway public key",
        Role::Agent => "an agent public key",
    }
}

/// Render raw key material in the textual form. Split out so the roundtrip test
/// can drive it with known bytes.
fn encode(prefix: &str, key: &[u8; KEY_LEN]) -> String {
    let mut bytes = Vec::with_capacity(KEY_LEN + 2);
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(&crc16_ccitt_false(key).to_be_bytes());
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
    )
}

/// Parse the textual form back into key material, verifying the prefix and the
/// checksum.
fn parse_with(
    prefix: &'static str,
    expected_what: &'static str,
    text: &str,
) -> Result<[u8; KEY_LEN], KeyError> {
    let text = text.trim();
    let body = text.strip_prefix(prefix).ok_or(KeyError::BadPrefix {
        expected_what,
        expected: prefix,
        found: describe(text),
    })?;
    if text.len() != TEXT_LEN {
        return Err(KeyError::BadLength(text.len()));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| KeyError::BadEncoding)?;
    // Guaranteed by the length check above, but the slicing below relies on it.
    if bytes.len() != KEY_LEN + 2 {
        return Err(KeyError::BadEncoding);
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes[..KEY_LEN]);
    let want = u16::from_be_bytes([bytes[KEY_LEN], bytes[KEY_LEN + 1]]);
    if crc16_ccitt_false(&key) != want {
        return Err(KeyError::BadChecksum);
    }
    Ok(key)
}

/// Name the kind of key `text` is, when it is one of the other three.
///
/// Only the prefix is looked at: the point is to answer "you pasted the wrong
/// one of these four", and a key that also fails its checksum is still that
/// answer.
fn describe(text: &str) -> Option<&'static str> {
    PREFIXES
        .iter()
        .find(|(prefix, _)| text.starts_with(prefix))
        .map(|(_, what)| *what)
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

    const ROLES: [Role; 2] = [Role::Gateway, Role::Agent];

    /// What one config field does with the text it was given.
    type Parse = fn(&str) -> Result<[u8; KEY_LEN], KeyError>;

    /// Every (prefix, parser) pair there is, for the tests that have to cover
    /// all four kinds rather than a representative one.
    fn kinds() -> Vec<(&'static str, Parse)> {
        vec![
            ("rxgs", |t| parse_private(Role::Gateway, t)),
            ("rxgp", |t| parse_public(Role::Gateway, t)),
            ("rxas", |t| parse_private(Role::Agent, t)),
            ("rxap", |t| parse_public(Role::Agent, t)),
        ]
    }

    /// A valid key of every kind, keyed by prefix.
    fn one_of_each() -> Vec<(&'static str, String)> {
        ROLES
            .iter()
            .flat_map(|&role| {
                let private = generate_private(role);
                let public = public_text_of(role, &private).unwrap();
                [
                    (role.private_prefix(), private),
                    (role.public_prefix(), public),
                ]
            })
            .collect()
    }

    // The published CRC16/CCITT-FALSE check value: "123456789" -> 0x29B1.
    #[test]
    fn crc_matches_the_reference_check_value() {
        assert_eq!(crc16_ccitt_false(b"123456789"), 0x29B1);
        assert_eq!(crc16_ccitt_false(b""), 0xFFFF);
    }

    #[test]
    fn every_prefix_is_distinct_and_the_documented_length() {
        let mut seen = Vec::new();
        for (prefix, _) in PREFIXES {
            assert_eq!(prefix.len(), PREFIX_LEN, "{prefix}");
            assert!(!seen.contains(&prefix), "{prefix} appears twice");
            seen.push(prefix);
        }
        // The table and the accessors have to agree, or `describe` would name a
        // key by a prefix no parser accepts.
        for role in ROLES {
            assert!(seen.contains(&role.private_prefix()));
            assert!(seen.contains(&role.public_prefix()));
        }
    }

    #[test]
    fn generated_keys_have_the_documented_shape() {
        for (prefix, key) in one_of_each() {
            assert_eq!(key.len(), TEXT_LEN, "{key}");
            assert!(key.starts_with(prefix), "{key} should start with {prefix}");
            // base64url: no '+', '/' or '=' that would need shell quoting.
            assert!(
                key[PREFIX_LEN..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{key}"
            );
        }
    }

    #[test]
    fn generate_parse_roundtrips_and_is_not_constant() {
        for role in ROLES {
            let a = generate_private(role);
            let b = generate_private(role);
            assert_ne!(a, b, "two generated keys must not collide");
            assert_ne!(
                parse_private(role, &a).unwrap(),
                parse_private(role, &b).unwrap()
            );
            let known = encode(role.private_prefix(), &[7u8; KEY_LEN]);
            assert_eq!(parse_private(role, &known).unwrap(), [7u8; KEY_LEN]);
        }
    }

    // The whole point of putting the role in the prefix: each field takes one
    // kind and refuses the other three by name.
    #[test]
    fn each_of_the_four_kinds_is_rejected_by_the_other_three_fields() {
        let keys = one_of_each();
        for (prefix, parse) in kinds() {
            for (kind, key) in &keys {
                let parsed = parse(key);
                if *kind == prefix {
                    assert!(parsed.is_ok(), "{prefix} must accept its own key: {key}");
                    continue;
                }
                assert!(
                    matches!(parsed, Err(KeyError::BadPrefix { .. })),
                    "{prefix} accepted a {kind} key: {parsed:?}"
                );
            }
        }
    }

    #[test]
    fn a_wrong_kind_is_named_in_the_error() {
        // The mistake this is here for: the two public keys are on screen
        // together while pairing, and they are the pair most easily swapped.
        let gateway = public_text_of(Role::Gateway, &generate_private(Role::Gateway)).unwrap();
        let err = parse_public(Role::Agent, &gateway).unwrap_err();
        assert_eq!(
            err,
            KeyError::BadPrefix {
                expected_what: "an agent public key",
                expected: "rxap",
                found: Some("a gateway public key"),
            }
        );
        let message = err.to_string();
        assert!(message.contains("rxap"), "{message}");
        assert!(message.contains("this is a gateway public key"), "{message}");

        // Something that is not a key at all has nothing to name.
        let err = parse_public(Role::Agent, "hunter2").unwrap_err();
        assert!(
            matches!(err, KeyError::BadPrefix { found: None, .. }),
            "{err:?}"
        );
        assert!(!err.to_string().contains("this is"), "{err}");
    }

    #[test]
    fn public_of_matches_snows_own_keypair() {
        // snow generates the private key and derives the public one together;
        // this crate stores only the private half and derives on demand. If the
        // two disagreed, every handshake would fail with a pubkey nobody could
        // trace back to a config file.
        let builder = snow::Builder::new(crate::noise::PARAMS.parse().unwrap());
        let pair = builder.generate_keypair().unwrap();
        let private: [u8; KEY_LEN] = pair.private.try_into().unwrap();
        assert_eq!(public_of(&private).as_slice(), pair.public.as_slice());
    }

    #[test]
    fn public_of_is_deterministic_and_key_specific() {
        let a = parse_private(Role::Agent, &generate_private(Role::Agent)).unwrap();
        let b = parse_private(Role::Agent, &generate_private(Role::Agent)).unwrap();
        assert_eq!(public_of(&a), public_of(&a));
        assert_ne!(public_of(&a), public_of(&b));
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let key = generate_private(Role::Agent);
        assert_eq!(
            parse_private(Role::Agent, &format!("  {key}\n")).unwrap(),
            parse_private(Role::Agent, &key).unwrap()
        );
    }

    #[test]
    fn bad_length_is_rejected() {
        for (prefix, parse) in kinds() {
            let key = encode(prefix, &[3u8; KEY_LEN]);
            assert_eq!(
                parse(&key[..TEXT_LEN - 1]),
                Err(KeyError::BadLength(TEXT_LEN - 1))
            );
            assert_eq!(
                parse(&format!("{key}A")),
                Err(KeyError::BadLength(TEXT_LEN + 1))
            );
        }
    }

    #[test]
    fn bad_base64_is_rejected() {
        // '*' is outside the base64url alphabet; length stays correct.
        let key = generate_private(Role::Gateway);
        let mangled = format!("{}*", &key[..TEXT_LEN - 1]);
        assert_eq!(mangled.len(), TEXT_LEN);
        assert_eq!(
            parse_private(Role::Gateway, &mangled),
            Err(KeyError::BadEncoding)
        );
    }

    // The point of the CRC: a single transposed character is caught here
    // rather than surfacing as a handshake failure against the other machine.
    #[test]
    fn a_single_character_typo_is_caught_by_the_checksum() {
        for (prefix, parse) in kinds() {
            let key = encode(prefix, &[9u8; KEY_LEN]);
            let mut chars: Vec<char> = key.chars().collect();
            // Well past the prefix and well before the CRC tail.
            chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
            let typo: String = chars.into_iter().collect();
            assert_eq!(parse(&typo), Err(KeyError::BadChecksum), "{prefix}");
        }
    }
}
