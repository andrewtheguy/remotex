//! The targets' passwords, kept encrypted in the config that names them.
//!
//! A target names its password (`password_ref = "win11"`) rather than writing it,
//! and the config's `[passwords]` table holds each one as an AES-256-GCM value:
//!
//! ```toml
//! [master]
//! salt = "…"
//! check = "…"
//!
//! [passwords]
//! win11 = "$RX;1;AES-256-GCM;…"
//! ```
//!
//! The key is never in the file. Argon2id derives it from the account's **master
//! password** and the file's own `[master].salt`, so one master password opens
//! every config an account keeps — the served gateway's and each local instance's —
//! while no two of them share a key. `[master].check` is an HMAC under that key, so
//! a wrong master password is told as one at start-up, before any value is tried.
//! A value is the password alone: what it is called is the config's to say, so
//! `remotex password encrypt` prints one to paste under whatever name a target
//! gives it.
//!
//! Where the master password comes from is the deployment's to say, and each
//! audience has its own answer — `[server].master_password_file` for a served
//! gateway, the keychain for the TUI (`crate::keychain`). This module only turns
//! it into a [`Key`] and uses that key.

use aes_gcm::aead::{Aead as _, KeyInit as _};
use aes_gcm::Aes256Gcm;
use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac as _};
use serde::Deserialize;
use sha2::Sha256;
use std::path::{Path, PathBuf};

/// What every `[passwords]` value starts with: the program, the format's version
/// and the cipher, so a value says what it is to anyone reading the file.
pub const PREFIX: &str = "$RX;1;AES-256-GCM;";

/// Argon2id's cost: 64 MiB, three passes, one lane — RFC 9106's second
/// recommendation, a fraction of a second on a laptop, paid once per start and once
/// per `remotex password` run.
const ARGON2_MEMORY_KIB: u32 = 64 * 1024;
const ARGON2_PASSES: u32 = 3;
const ARGON2_LANES: u32 = 1;

/// What `[master].check` authenticates. A constant, because the check proves the
/// key and nothing else.
const CHECK_MESSAGE: &[u8] = b"remotex master password check";

const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// The `[master]` table as written: the salt the key is derived with, and the check
/// that tells a wrong master password apart.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MasterSection {
    /// 32 random bytes, base64, printed by `remotex password master`.
    pub salt: String,
    /// HMAC-SHA256 of a fixed message under the derived key, base64.
    pub check: String,
}

impl MasterSection {
    /// The salt and check decode to what they should, so a damaged block is
    /// refused when the file is read rather than told as a wrong password later.
    pub fn validate(&self) -> anyhow::Result<()> {
        let salt = BASE64.decode(&self.salt).context("[master].salt is not base64")?;
        anyhow::ensure!(salt.len() == SALT_LEN, "[master].salt is not {SALT_LEN} bytes");
        let check = BASE64.decode(&self.check).context("[master].check is not base64")?;
        anyhow::ensure!(check.len() == 32, "[master].check is not 32 bytes");
        Ok(())
    }
}

/// The account's master password, as typed or as kept.
#[derive(Clone)]
pub struct MasterPassword(String);

// Manual Debug, so nothing that formats one prints it.
impl std::fmt::Debug for MasterPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterPassword(..)")
    }
}

impl MasterPassword {
    pub fn new(password: String) -> anyhow::Result<Self> {
        anyhow::ensure!(!password.is_empty(), "the master password is empty");
        Ok(Self(password))
    }

    /// A master password nobody will ever type: 32 random bytes, for a keychain
    /// to keep.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes[..]);
        Self(BASE64.encode(bytes))
    }

    /// Read a master password kept in a file: a container's secret, a systemd
    /// credential. One trailing line ending is the file's, not the password's.
    pub fn read_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the master password file {}", path.display()))?;
        let password = text.strip_suffix('\n').unwrap_or(&text);
        let password = password.strip_suffix('\r').unwrap_or(password);
        Self::new(password.to_owned())
            .with_context(|| format!("in the master password file {}", path.display()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One config's key: the master password under that file's salt.
#[derive(Clone)]
pub struct Key([u8; 32]);

// Manual Debug, for the reason [`MasterPassword`]'s is.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    /// A new `[master]` block for `master`, with the key it describes.
    pub fn create(master: &MasterPassword) -> anyhow::Result<(MasterSection, Self)> {
        let mut salt = [0u8; SALT_LEN];
        rand::fill(&mut salt[..]);
        let key = Self::stretch(master, &salt)?;
        let section = MasterSection {
            salt: BASE64.encode(salt),
            check: BASE64.encode(key.check()),
        };
        Ok((section, key))
    }

    /// The key `section` was made with, if `master` is the password it was made
    /// from.
    pub fn derive(master: &MasterPassword, section: &MasterSection) -> anyhow::Result<Self> {
        section.validate()?;
        let salt = BASE64.decode(&section.salt).context("[master].salt is not base64")?;
        let check = BASE64.decode(&section.check).context("[master].check is not base64")?;
        let key = Self::stretch(master, &salt)?;
        let mut mac = key.mac();
        mac.update(CHECK_MESSAGE);
        mac.verify_slice(&check)
            .map_err(|_| anyhow::anyhow!("the master password does not open this config"))?;
        Ok(key)
    }

    fn stretch(master: &MasterPassword, salt: &[u8]) -> anyhow::Result<Self> {
        let params = argon2::Params::new(ARGON2_MEMORY_KIB, ARGON2_PASSES, ARGON2_LANES, Some(32))
            .map_err(|error| anyhow::anyhow!("invalid Argon2 parameters: {error}"))?;
        let mut key = [0u8; 32];
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password_into(master.as_str().as_bytes(), salt, &mut key)
            .map_err(|error| anyhow::anyhow!("Argon2id failed: {error}"))?;
        Ok(Self(key))
    }

    fn mac(&self) -> Hmac<Sha256> {
        Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC takes any key length")
    }

    fn check(&self) -> [u8; 32] {
        let mut mac = self.mac();
        mac.update(CHECK_MESSAGE);
        mac.finalize().into_bytes().into()
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.0).expect("a 32-byte key")
    }

    /// A `[passwords]` value for `password`.
    pub fn encrypt(&self, password: &str) -> anyhow::Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce[..]);
        let sealed = self
            .cipher()
            .encrypt(&nonce.into(), password.as_bytes())
            .map_err(|_| anyhow::anyhow!("cannot encrypt the password"))?;
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&sealed);
        Ok(format!("{PREFIX}{}", BASE64.encode(blob)))
    }

    /// The password `[passwords].<name>` holds; `name` is for the error.
    pub fn decrypt(&self, name: &str, value: &str) -> anyhow::Result<String> {
        let blob = decode_value(name, value)?;
        let (nonce, sealed) = blob.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce's length");
        let plain = self
            .cipher()
            .decrypt(&nonce.into(), sealed)
            .map_err(|_| {
                anyhow::anyhow!(
                    "[passwords].{name} does not open under this config's master password; \
                     it was encrypted under another [master] — encrypt it again with \
                     `remotex password encrypt`"
                )
            })?;
        String::from_utf8(plain).with_context(|| format!("[passwords].{name} is not UTF-8"))
    }

    /// The key as one line, for a control plane to hand its worker down a pipe.
    pub fn to_line(&self) -> String {
        BASE64.encode(self.0)
    }

    /// The key [`Self::to_line`] wrote.
    pub fn from_line(line: &str) -> anyhow::Result<Self> {
        let bytes = BASE64.decode(line.trim()).context("the key is not base64")?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("the key is not 32 bytes"))?;
        Ok(Self(key))
    }
}

/// A `[passwords]` value's sealed bytes, nonce first.
fn decode_value(name: &str, value: &str) -> anyhow::Result<Vec<u8>> {
    let encoded = value.strip_prefix(PREFIX).with_context(|| {
        format!(
            "[passwords].{name} is not an encrypted value (it starts with {PREFIX:?}): paste \
             what `remotex password encrypt` prints rather than the password"
        )
    })?;
    let blob = BASE64
        .decode(encoded)
        .with_context(|| format!("[passwords].{name} is not base64 after its prefix"))?;
    anyhow::ensure!(
        blob.len() >= NONCE_LEN + TAG_LEN,
        "[passwords].{name} is too short to be an encrypted value"
    );
    Ok(blob)
}

/// `[passwords].<name>` has the shape of a value [`Key::encrypt`] wrote, checked
/// without the key — which is all reading a file, or `check-config`, has.
pub fn check_value(name: &str, value: &str) -> anyhow::Result<()> {
    decode_value(name, value).map(|_| ())
}

/// A password's name: what `password_ref` says and `[passwords]` keys by. Letters,
/// digits, `-` and `_`, so it is a bare TOML key and a shell word as it stands.
pub fn valid_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "password name {name:?} must be 1 to 64 letters, digits, '-' or '_'"
    );
    Ok(())
}

/// Give the config at `path` its `[master]` block under `master`, keeping the rest
/// of the file as written. A file that has one keeps it: replacing it would leave
/// every value in `[passwords]` sealed under a key nothing can derive any more.
pub fn init_file(path: &Path, master: &MasterPassword) -> anyhow::Result<()> {
    let mut doc = read_document(path)?;
    anyhow::ensure!(
        !doc.contains_key("master"),
        "{} already has a [master] block",
        path.display()
    );
    let (section, _) = Key::create(master)?;
    let mut table = toml_edit::Table::new();
    table.insert("salt", toml_edit::value(section.salt));
    table.insert("check", toml_edit::value(section.check));
    doc.insert("master", toml_edit::Item::Table(table));
    write_document(path, &doc)
}

/// Encrypt `password` into `[passwords].<name>` of the config at `path`, under the
/// key its `[master]` block and `master` give, replacing any value of that name.
pub fn set_file(path: &Path, name: &str, password: &str, master: &MasterPassword) -> anyhow::Result<()> {
    valid_name(name)?;
    anyhow::ensure!(!password.is_empty(), "the password is empty");
    let key = Key::derive(master, &master_of(path)?).with_context(|| format!("in {}", path.display()))?;
    let mut doc = read_document(path)?;
    let value = key.encrypt(password)?;
    let passwords = doc
        .entry("passwords")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .context("[passwords] is not a table")?;
    passwords.insert(name, toml_edit::value(value));
    write_document(path, &doc)
}

/// The `[master]` block of the config at `path`, read and left as it is.
pub fn master_of(path: &Path) -> anyhow::Result<MasterSection> {
    /// The one table wanted, whatever else the file holds.
    #[derive(Deserialize)]
    struct Only {
        master: Option<MasterSection>,
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let only: Only = toml::from_str(&text)
        .with_context(|| format!("{} is not valid TOML", path.display()))?;
    let section = only.master.with_context(|| {
        format!(
            "{} has no [master] block — paste the one `remotex password master` prints",
            path.display()
        )
    })?;
    section.validate().with_context(|| format!("invalid [master] in {}", path.display()))?;
    Ok(section)
}

/// `[server].master_password_file` of the config at `path`, taken from the file's
/// directory, when it names one.
pub fn master_file_of(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let doc = read_document(path)?;
    Ok(doc
        .get("server")
        .and_then(|server| server.get("master_password_file"))
        .and_then(|file| file.as_str())
        .map(|file| path.parent().unwrap_or(Path::new("")).join(file)))
}

fn read_document(path: &Path) -> anyhow::Result<toml_edit::DocumentMut> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    text.parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))
}

/// Replace the file whole: a temporary file beside it, given its permissions, then
/// renamed over it, so a process stopped half way leaves the old file and never
/// half of the new one.
fn write_document(path: &Path, doc: &toml_edit::DocumentMut) -> anyhow::Result<()> {
    let permissions = std::fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .permissions();
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options
        .open(&temporary)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, doc.to_string().as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::set_permissions(&temporary, permissions))
        .and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master(text: &str) -> MasterPassword {
        MasterPassword::new(text.to_owned()).unwrap()
    }

    #[test]
    fn a_value_opens_under_its_own_master_only() {
        let (section, key) = Key::create(&master("correct horse")).unwrap();
        let value = key.encrypt("hunter2").unwrap();
        assert!(value.starts_with(PREFIX), "{value}");
        check_value("win11", &value).unwrap();
        assert_ne!(key.encrypt("hunter2").unwrap(), value, "every value has its own nonce");

        let again = Key::derive(&master("correct horse"), &section).unwrap();
        assert_eq!(again.decrypt("win11", &value).unwrap(), "hunter2");

        let err = Key::derive(&master("battery staple"), &section).unwrap_err();
        assert!(format!("{err:#}").contains("does not open"), "{err:#}");
    }

    #[test]
    fn two_configs_under_one_master_share_no_key() {
        let (_, first) = Key::create(&master("one master")).unwrap();
        let (_, second) = Key::create(&master("one master")).unwrap();
        let value = first.encrypt("hunter2").unwrap();
        assert!(second.decrypt("win11", &value).is_err());
    }

    #[test]
    fn the_key_survives_the_handover_line() {
        let (_, key) = Key::create(&master("m")).unwrap();
        let value = key.encrypt("b").unwrap();
        let line = key.to_line();
        assert!(!line.contains('\n'));
        assert_eq!(Key::from_line(&format!("{line}\n")).unwrap().decrypt("a", &value).unwrap(), "b");
    }

    #[test]
    fn a_plaintext_value_is_refused_by_shape() {
        let err = check_value("win11", "hunter2").unwrap_err();
        assert!(format!("{err:#}").contains("remotex password encrypt"), "{err:#}");
    }

    #[test]
    fn a_master_file_loses_one_line_ending_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master");
        std::fs::write(&path, "pass word \r\n").unwrap();
        assert_eq!(MasterPassword::read_file(&path).unwrap().as_str(), "pass word ");
        std::fs::write(&path, "\n").unwrap();
        assert!(MasterPassword::read_file(&path).is_err(), "an empty password is refused");
    }

    #[test]
    fn a_config_gets_its_master_and_passwords_written_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotex.toml");
        let text = "# kept\n[[targets]]\nname = \"win\" # also kept\nprotocol = \"rdp\"\nhost = \"h\"\nusername = \"u\"\npassword_ref = \"win11\"\n";
        std::fs::write(&path, text).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();

        let master = master("one master");
        init_file(&path, &master).unwrap();
        assert!(init_file(&path, &master).is_err(), "a second [master] would orphan [passwords]");
        set_file(&path, "win11", "hunter2", &master).unwrap();
        assert!(set_file(&path, "win11", "x", &self::master("another")).is_err());

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# kept\n"), "{written}");
        assert!(written.contains("# also kept"), "{written}");
        assert!(!written.contains("hunter2"), "{written}");
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions()) & 0o777,
            0o600
        );

        let mut file = crate::config::ConfigFile::parse(&written).unwrap();
        assert!(file.needs_master());
        assert!(file.unlock(None).is_err(), "a file naming passwords needs its key");
        let key = file.key(&master).unwrap();
        file.unlock(Some(&key)).unwrap();
        assert_eq!(file.targets[0].password, "hunter2");
    }

    #[test]
    fn names_are_bare_keys() {
        valid_name("win11").unwrap();
        valid_name("mac_mini-2").unwrap();
        for bad in ["", "a b", "a.b", "ä", &"x".repeat(65)] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
    }
}
