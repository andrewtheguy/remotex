//! The agent's TOML config, in the house style of
//! `packaging/etc/remotex.toml.example`.
//!
//! Deliberately tiny. The agent has no notion of users, targets or sessions —
//! it listens on one port, accepts one gateway at a time, and shares one
//! display. Everything else is the gateway's business.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address to listen on. Defaults to every interface on the rxa port —
    /// the whole point is to be reachable from the gateway host.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Pre-shared key, matching the gateway target's `psk`. This is the entire
    /// credential; without it no handshake completes.
    pub psk: String,
    /// Which display to share, as an index into the shareable-display list.
    /// `0` is the main display.
    #[serde(default)]
    pub display: usize,
}

fn default_listen() -> String {
    format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT)
}

impl Config {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Config = toml::from_str(text).context("invalid TOML config")?;
        // Validated here so a mistyped key is a startup failure with the CRC's
        // "transcription typo" hint, not a handshake that mysteriously never
        // completes once the gateway starts dialing.
        rxa_proto::psk::parse(&config.psk).map_err(|e| anyhow::anyhow!("invalid psk: {e}"))?;
        anyhow::ensure!(!config.listen.is_empty(), "listen must not be empty");
        Ok(config)
    }

    /// The 32-byte key. Infallible after [`Config::parse`].
    pub fn psk_bytes(&self) -> [u8; 32] {
        rxa_proto::psk::parse(&self.psk).expect("psk validated in Config::parse")
    }
}

/// Where the config lives when `--config` is not given:
/// `~/Library/Application Support/remotex-agent/config.toml`.
///
/// A per-user path, unlike the gateway's system-wide `<prefix>/etc`, because the
/// agent is a LaunchAgent: it runs as the logged-in user, in that user's GUI
/// session, and its TCC grants belong to that user too (see the module docs in
/// `main.rs`).
pub fn default_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home)
        .join("Library/Application Support/remotex-agent/config.toml")
        .to_path_buf())
}

pub fn load(explicit: Option<&Path>) -> anyhow::Result<(Config, PathBuf)> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => default_path()?,
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config = Config::parse(&text).with_context(|| format!("in config file {}", path.display()))?;
    Ok((config, path))
}

/// Load the config, creating it with a freshly generated key if it is absent.
///
/// There is no install script to write this file (the app registers itself — see
/// [`crate::loginitem`]), so first launch has to be self-sufficient: the user
/// drags the bundle in, opens it, and the only thing left to do is copy the
/// printed key to the gateway.
///
/// Returns the config, its path, and whether this call created it.
pub fn load_or_create(explicit: Option<&Path>) -> anyhow::Result<(Config, PathBuf, bool)> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => default_path()?,
    };
    if path.exists() {
        let (config, path) = load(Some(&path))?;
        return Ok((config, path, false));
    }

    let parent = path
        .parent()
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let text = template(&rxa_proto::psk::generate());
    write_private(&path, &text)?;
    let config = Config::parse(&text).expect("the generated template must be valid");
    Ok((config, path, true))
}

/// Write a file readable only by its owner.
///
/// The key in here is the entire credential for reaching this Mac's screen and
/// keyboard, so the file is created 0600 from the start rather than chmod'ed
/// afterwards — that leaves no window where it is world-readable.
fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// The starting config, with `psk` filled in.
fn template(psk: &str) -> String {
    format!(
        r#"# remotex-agent configuration.
#
# The psk below is the entire credential: the gateway authenticates with it and
# nothing else, which is what makes a reconnect need no login. The *same* key
# must appear as `psk` on the matching [[targets]] entry in the gateway's
# remotex.toml.
#
# Print it again at any time with:  remotex-agent --show-psk

# Address to listen on. 0.0.0.0 so the gateway host can reach it; narrow this to
# a specific interface if you prefer.
listen = "0.0.0.0:{port}"

psk = "{psk}"

# Which display to share (0 is the main one).
#display = 0
"#,
        port = rxa_proto::DEFAULT_PORT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_psk(extra: &str) -> String {
        format!("psk = \"{}\"\n{extra}", rxa_proto::psk::generate())
    }

    #[test]
    fn minimal_config_listens_on_the_rxa_port_everywhere() {
        let config = Config::parse(&with_psk("")).unwrap();
        assert_eq!(config.listen, format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT));
        assert_eq!(config.display, 0, "the main display by default");
    }

    #[test]
    fn full_config_parses() {
        let psk = rxa_proto::psk::generate();
        let config =
            Config::parse(&format!("listen = \"192.168.1.5:9000\"\npsk = \"{psk}\"\ndisplay = 1"))
                .unwrap();
        assert_eq!(config.listen, "192.168.1.5:9000");
        assert_eq!(config.display, 1);
        assert_eq!(config.psk_bytes(), rxa_proto::psk::parse(&psk).unwrap());
    }

    #[test]
    fn a_missing_or_malformed_psk_is_rejected() {
        let err = Config::parse("listen = \"0.0.0.0:1\"").unwrap_err();
        assert!(format!("{err:#}").contains("psk"), "{err:#}");

        let err = Config::parse("psk = \"nonsense\"").unwrap_err();
        assert!(format!("{err:#}").contains("invalid psk"), "{err:#}");

        // A single-character typo is caught by the key's own checksum.
        let psk = rxa_proto::psk::generate();
        let mut chars: Vec<char> = psk.chars().collect();
        chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        let err = Config::parse(&format!("psk = \"{typo}\"")).unwrap_err();
        assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    }

    #[test]
    fn typos_in_keys_are_rejected() {
        // deny_unknown_fields: a misspelled key is an error, not silence.
        let err = Config::parse(&with_psk("listn = \"x\"")).unwrap_err();
        assert!(format!("{err:#}").contains("listn"), "{err:#}");
    }

    // First launch has to be self-sufficient: there is no install script to
    // write this file.
    #[test]
    fn a_missing_config_is_created_with_a_fresh_key() {
        let dir = std::env::temp_dir().join(format!("rxa-agent-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/config.toml");

        let (config, written, created) = load_or_create(Some(&path)).unwrap();
        assert!(created, "the first call should create the file");
        assert_eq!(written, path);
        assert!(path.exists());
        // A real, checksum-valid key, not a placeholder.
        assert_eq!(config.psk.len(), rxa_proto::psk::TEXT_LEN);
        rxa_proto::psk::parse(&config.psk).unwrap();
        assert_eq!(config.listen, format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT));

        // Owner-only: this file is the whole credential.
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "config must not be group/world readable");

        // A second call reuses it rather than minting a new key — otherwise
        // every restart would silently break the pairing with the gateway.
        let (again, _, created) = load_or_create(Some(&path)).unwrap();
        assert!(!created);
        assert_eq!(again.psk, config.psk);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_generated_template_parses() {
        // The template is written before it is ever parsed, so a typo in it
        // would only surface on a user's first launch.
        let psk = rxa_proto::psk::generate();
        let config = Config::parse(&template(&psk)).unwrap();
        assert_eq!(config.psk, psk);
        assert_eq!(config.display, 0);
    }

    #[test]
    fn the_default_config_path_is_per_user() {
        // The agent is a LaunchAgent, so its config belongs to the logged-in
        // user rather than a system-wide prefix.
        let path = default_path().unwrap();
        assert!(path.ends_with("Library/Application Support/remotex-agent/config.toml"), "{path:?}");
    }
}
