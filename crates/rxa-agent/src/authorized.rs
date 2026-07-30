//! The gateways this Mac will answer — a list, the way `~/.ssh/authorized_keys`
//! is a list.
//!
//! Its own file beside `config.toml`, and deliberately not a field in it. The
//! config is rewritten whole on every save (see [`crate::config`]), so nothing a
//! person adds to it by hand survives — which is the exactly wrong property for a
//! list people annotate, reorder and comment out a line of. So this module keeps
//! the file's **text**, not just the keys parsed out of it: an edit rewrites what
//! was typed, comments and blank lines included, and the parsed entries are a
//! reading of that text rather than a replacement for it.
//!
//! Being on the list decides whether a gateway may *ask* for the session; it
//! decides nothing about whose turn it is, which is [`crate::state::decide`] and is
//! keyed on a session id. Two gateways can be listed here, and one holds the Mac at
//! a time.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use rxa_proto::key::{self, Role};

/// The file's name, beside `config.toml`.
pub const FILE_NAME: &str = "authorized_gateways";

/// What a brand-new list looks like: the format, and nothing on it.
///
/// Seeded rather than left blank because this file is edited as text, and an empty
/// text view says nothing about what belongs in it. The header is ordinary content
/// from then on — it is kept because the user's own save keeps it, not because
/// anything here re-imposes it.
const TEMPLATE: &str = "\
# Gateways allowed to reach this Mac, one per line:
#
#   <gateway public key> <a name for the machine it belongs to>
#
# `remotex rxa-pubkey` prints a gateway's key (rxgp…). Neither key in a pairing is
# a secret; the name is only for this Mac, and is what its menu bar calls that
# gateway while it is connected.
#
# Blank lines and lines starting with # are ignored, so an entry can be commented
# out and put back. While this list is empty every connection is refused.
";

/// One line of the list: a gateway's public key, and what to call it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// `rxgp…`, as written on the line. Valid — [`Authorized::parse`] rejected the
    /// file otherwise.
    pub key: String,
    /// The rest of the line, trimmed. May be empty: a key on its own is a
    /// perfectly good entry, and then there is nothing to call it but its address.
    pub comment: String,
}

impl Entry {
    /// The 32 key bytes. Infallible after [`Authorized::parse`].
    pub fn key_bytes(&self) -> [u8; 32] {
        key::parse_public(Role::Gateway, &self.key).expect("validated in Authorized::parse")
    }

    /// What to call this gateway, or `None` when the line carried no comment.
    pub fn name(&self) -> Option<&str> {
        Some(self.comment.trim()).filter(|name| !name.is_empty())
    }
}

/// The file: its text as a person wrote it, and the entries read out of it.
///
/// The two are kept together rather than the text being re-derived, because
/// rendering a list of entries back out would silently delete every comment and
/// blank line in it — see the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorized {
    text: String,
    entries: Vec<Entry>,
}

impl Default for Authorized {
    /// An empty list, with the format written out for whoever opens it. This is
    /// what a Mac nobody has authorized anything on has.
    fn default() -> Self {
        Self {
            text: TEMPLATE.to_owned(),
            entries: Vec::new(),
        }
    }
}

impl Authorized {
    /// Read a list, keeping the text verbatim.
    ///
    /// Errors name the line number, because that is the only way an editor's worth
    /// of lines can be corrected: "line 4: key checksum mismatch" points at
    /// something, where "invalid key" does not.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let text = normalize(text);
        let mut entries: Vec<Entry> = Vec::new();
        let mut seen: HashSet<[u8; 32]> = HashSet::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let number = n + 1;
            let (key, comment) = match line.split_once(char::is_whitespace) {
                Some((key, comment)) => (key, comment.trim()),
                None => (line, ""),
            };
            let bytes = key::parse_public(Role::Gateway, key)
                .map_err(|e| anyhow::anyhow!("line {number}: {e}"))?;
            // A repeated key is always a mistake rather than a configuration: the
            // second entry's name could never be reported, so the file would be
            // quietly not saying what it appears to say.
            anyhow::ensure!(
                seen.insert(bytes),
                "line {number}: this key is already on the list"
            );
            entries.push(Entry {
                key: key.to_owned(),
                comment: comment.to_owned(),
            });
        }
        Ok(Self { text, entries })
    }

    /// The file as it stands, which is what an editor shows and what a save writes.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Whether this Mac will answer anybody at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The entry a dialing gateway's key matches, from the handshake.
    ///
    /// A scan, because the list is a handful of lines that somebody types by hand;
    /// an index would be a map built per connection to search four entries.
    pub fn lookup(&self, key: &[u8; 32]) -> Option<&Entry> {
        self.entries.iter().find(|entry| &entry.key_bytes() == key)
    }

    /// Load the list, treating a missing file as an empty one.
    ///
    /// Missing is the ordinary first-launch state and not an error: the agent has
    /// to run before anybody can read its public key out of the menu bar, so it
    /// necessarily starts with nothing authorized. It listens and refuses.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text)
                .with_context(|| format!("in authorized gateways file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Write the list, replacing whatever is there.
    ///
    /// Atomic by way of a temporary file and a rename, like the config beside it: a
    /// half-written list is a Mac that refuses the gateway it was just told to
    /// accept. Owner-only, not because the keys are secret — they are public halves
    /// — but because anything that can *append* to this file can reach this screen,
    /// which is the same reason `sshd` refuses a group-writable `authorized_keys`.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let name = path
            .file_name()
            .context("authorized gateways path has no file name")?
            .to_string_lossy()
            .into_owned();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let temp = path.with_file_name(format!(".{name}.new"));
        crate::config::write_private(&temp, &self.text)?;
        if let Err(e) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(e).with_context(|| format!("failed to replace {}", path.display()));
        }
        Ok(())
    }
}

/// Where the list lives: beside the config it belongs to.
///
/// Derived from the config's path rather than fixed, so `--config tmp/agent.toml`
/// reads `tmp/authorized_gateways` — one `--config` moves the whole of an agent's
/// state, which is what makes a test setup separable from the real one.
pub fn path_beside(config: &Path) -> PathBuf {
    config.with_file_name(FILE_NAME)
}

/// `\r\n` out and exactly one trailing newline in.
///
/// Both halves are about equality rather than parsing. The text is compared to
/// decide whether a save changed anything, and a text view hands back what the user
/// typed — which may or may not end in a newline, and may carry CRLF from a paste.
/// Without this, saving an untouched list could report a change and restart the
/// agent for nothing.
fn normalize(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = text.trim_end_matches('\n');
    if trimmed.is_empty() {
        return String::new();
    }
    format!("{trimmed}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::scratch::TempDir;

    fn gateway_key() -> String {
        key::public_text_of(Role::Gateway, &key::generate_private(Role::Gateway)).unwrap()
    }

    #[test]
    fn a_key_and_a_name_per_line() {
        let (home, laptop) = (gateway_key(), gateway_key());
        let list = Authorized::parse(&format!(
            "{home} home server\n{laptop} the laptop in the study\n"
        ))
        .unwrap();

        assert_eq!(list.len(), 2);
        assert!(!list.is_empty());
        assert_eq!(list.entries()[0].key, home);
        assert_eq!(list.entries()[0].name(), Some("home server"));
        // The whole of the rest of the line, spaces and all: a name is prose, not
        // an identifier.
        assert_eq!(list.entries()[1].name(), Some("the laptop in the study"));
        assert_eq!(
            list.lookup(&key::parse_public(Role::Gateway, &home).unwrap())
                .unwrap()
                .name(),
            Some("home server")
        );
    }

    // The point of the format: an entry can be parked without being lost, and a
    // person's own notes stay in the file.
    #[test]
    fn comments_and_blank_lines_are_ignored_and_kept() {
        let (live, parked) = (gateway_key(), gateway_key());
        let text = format!(
            "# the machines that may reach this Mac\n\n{live} home server\n\n\
             # away for now:\n#{parked} the old laptop\n"
        );
        let list = Authorized::parse(&text).unwrap();

        assert_eq!(list.len(), 1, "a commented-out entry is not an entry");
        assert_eq!(list.entries()[0].key, live);
        assert!(
            list.lookup(&key::parse_public(Role::Gateway, &parked).unwrap())
                .is_none()
        );
        // And nothing was thrown away: this text is what a save writes back.
        assert_eq!(list.text(), text);
        assert!(list.text().contains("away for now"));
    }

    #[test]
    fn a_key_with_no_name_is_still_an_entry() {
        let key = gateway_key();
        let list = Authorized::parse(&format!("  {key}  \n")).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list.entries()[0].key, key);
        // Nothing to call it, which the menu bar answers with the address instead.
        assert_eq!(list.entries()[0].name(), None);
    }

    #[test]
    fn an_absent_or_empty_list_authorizes_nobody() {
        assert!(Authorized::parse("").unwrap().is_empty());
        assert!(Authorized::parse("# nothing yet\n").unwrap().is_empty());
        assert!(Authorized::default().is_empty());
        // The default carries the format for whoever opens it, rather than being a
        // blank the user has to guess at.
        assert!(Authorized::default().text().contains("rxgp"));
    }

    // Every rejection names its line, because the file is edited as a block of
    // text and "invalid key" would point at none of it.
    #[test]
    fn a_bad_line_is_rejected_by_number() {
        let good = gateway_key();

        let err = Authorized::parse(&format!("{good} home\nrxgpnope laptop\n")).unwrap_err();
        assert!(format!("{err:#}").contains("line 2"), "{err:#}");

        // A single-character typo, caught by the key's own checksum rather than
        // ten seconds later in a handshake.
        let mut chars: Vec<char> = good.chars().collect();
        chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        let err = Authorized::parse(&format!("# a header\n\n{typo} home\n")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("line 3"), "{msg}");
        assert!(msg.contains("checksum"), "{msg}");
    }

    // The four prefixes exist for this moment: three of the four keys in play are
    // wrong here, and each is a plausible paste.
    #[test]
    fn only_a_gateway_public_key_belongs_on_the_list() {
        let agent_private = key::generate_private(Role::Agent);
        for (bad, expect) in [
            (
                key::public_text_of(Role::Agent, &agent_private).unwrap(),
                "an agent public key",
            ),
            (
                key::generate_private(Role::Gateway),
                "a gateway private key",
            ),
            (agent_private.clone(), "an agent private key"),
        ] {
            let err = Authorized::parse(&format!("{bad} somewhere\n")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("line 1"), "{bad} gave {msg}");
            assert!(msg.contains(expect), "{bad} gave {msg}");
        }
    }

    // A duplicate is not a configuration: the second name could never be reported,
    // so the file would look like it says something it does not.
    #[test]
    fn the_same_key_twice_is_refused() {
        let key = gateway_key();
        let err = Authorized::parse(&format!("{key} home\n{key} home again\n")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("line 2"), "{msg}");
        assert!(msg.contains("already"), "{msg}");
    }

    // Saving is how the Manage panel lands an edit, and the text is compared to
    // decide whether the agent has to restart into it — so a round trip has to be
    // the identity, including through a text view that drops the last newline.
    #[test]
    fn saving_round_trips_and_stays_owner_only() {
        let dir = TempDir::new("authorized-save");
        let path = dir.join(FILE_NAME);
        assert_eq!(path_beside(&dir.join("config.toml")), path);

        // Nothing there yet: the ordinary first launch.
        assert_eq!(Authorized::load(&path).unwrap(), Authorized::default());

        let key = gateway_key();
        let list = Authorized::parse(&format!("# mine\n{key} home server")).unwrap();
        list.save(&path).unwrap();
        assert_eq!(mode(&path), 0o600, "anything that can append here can watch");
        assert_eq!(Authorized::load(&path).unwrap(), list);
        assert!(list.text().ends_with("home server\n"), "{:?}", list.text());

        // CRLF from a paste, and no trailing newline from the text view, are the
        // same list — or an untouched save would restart the agent for nothing.
        let retyped = Authorized::parse(&format!("# mine\r\n{key} home server\r\n\r\n")).unwrap();
        assert_eq!(retyped, list);

        // And no temporary file left holding a copy.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != FILE_NAME)
            .collect();
        assert!(strays.is_empty(), "left behind {strays:?}");
    }

    // A file somebody hand-edited into nonsense has to be a legible refusal that
    // names the file, not a mystery about why the gateway stopped connecting.
    #[test]
    fn loading_a_broken_file_names_it() {
        let dir = TempDir::new("authorized-broken");
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "rxgpnope home\n").unwrap();
        let err = Authorized::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(FILE_NAME), "{msg}");
        assert!(msg.contains("line 1"), "{msg}");
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}
