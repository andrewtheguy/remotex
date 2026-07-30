//! GUI-owned agent configuration.
//!
//! The menu bar rewrites the whole file, so rendered comments are canonical and
//! manual additions are not retained. The agent listens for one paired gateway
//! and optionally creates one virtual display.
//!
//! One *paired* gateway is what this file says, and it is not the same statement
//! as one session at a time — that is `crate::state`, keyed on the session id a
//! connection claims with rather than on the key below. A list of permitted
//! gateways would change this file and the handshake, and leave the session slot
//! alone (`docs/roadmap.md`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address to listen on. Defaults to every interface on the rxa port —
    /// the whole point is to be reachable from the gateway host.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// This Mac's own private key (`rxas…`), minted on first launch and never
    /// shown, copied or printed. Its public half — derived on demand by
    /// [`Config::public_key`] — is what goes on the gateway.
    pub private_key: String,
    /// The gateway's public key (`rxgp…`), from `remotex rxa-pubkey`. The agent
    /// answers that gateway and no other — a statement about who may *ask* for a
    /// session, not about how many there can be (see [`crate::state::decide`]).
    ///
    /// **May be empty**, which means unpaired: a first launch has nobody to
    /// pair with yet, and the agent has to be running before its own public key
    /// can be read out of the menu bar. An unpaired agent listens and refuses
    /// every connection — see [`Config::gateway_public_key_bytes`].
    #[serde(default)]
    pub gateway_public_key: String,
    /// Give the Mac an extra display, of the agent's own making.
    ///
    /// An addition, not a substitution: `CGVirtualDisplay` *adds* a monitor next
    /// to the ones already attached. So this only decides whether that display
    /// exists — which of them a session shares is chosen from the viewer or the
    /// browser, per session, and is no business of this file.
    ///
    /// Off by default, and still a setting rather than something a client can
    /// ask for: a display appearing and disappearing rearranges the windows on
    /// it, so it is created once at startup and lives as long as the agent.
    #[serde(default)]
    pub virtual_display: bool,
    /// Creation size in points (`WIDTHxHEIGHT`, minimum 800x600). It fixes the
    /// virtual display's immutable pixel/density envelope; later client resizes
    /// are clamped to it, while macOS may remember a different current mode.
    #[serde(default = "default_virtual_display_initial_size")]
    pub virtual_display_initial_size: String,
}

fn default_listen() -> String {
    format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT)
}

/// 1280x800 points — 2560x1600 pixels once doubled, 4.1 megapixels a full frame.
///
/// The encode cost is the reason not to reach higher by default; the ceiling is
/// what makes a smaller mode picked in System Settings cost proportionally less.
fn default_virtual_display_initial_size() -> String {
    "1280x800".to_owned()
}

impl Config {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Config = toml::from_str(text).context("invalid TOML config")?;
        config.validate()?;
        Ok(config)
    }

    /// Everything that must hold before a config is served or saved.
    ///
    /// Called on the way in *and* on the way out: the menu bar validates an
    /// edited value with this before it is written, so a mistyped address is a
    /// panel the user can correct rather than a handshake that mysteriously
    /// never completes once the gateway starts dialing.
    pub fn validate(&self) -> anyhow::Result<()> {
        rxa_proto::key::parse_private(rxa_proto::key::Role::Agent, &self.private_key)
            .map_err(|e| anyhow::anyhow!("invalid private_key: {e}"))?;
        // Empty is unpaired, which is what a first launch looks like. Anything
        // else has to be a gateway's public key — including, pointedly, not
        // this Mac's own, which the role in the prefix is there to catch.
        if !self.gateway_public_key.trim().is_empty() {
            rxa_proto::key::parse_public(
                rxa_proto::key::Role::Gateway,
                &self.gateway_public_key,
            )
            .map_err(|e| anyhow::anyhow!("invalid gateway_public_key: {e}"))?;
        }
        self.socket_addr()?;
        self.virtual_display_initial_points()?;
        Ok(())
    }

    /// Whether this agent has been told which gateway to answer.
    pub fn is_paired(&self) -> bool {
        !self.gateway_public_key.trim().is_empty()
    }

    /// The virtual display's size in points.
    ///
    /// Validated even when `virtual_display` is off, so switching it on later
    /// cannot be the moment a typo in the size is discovered.
    pub fn virtual_display_initial_points(&self) -> anyhow::Result<(u32, u32)> {
        let text = self.virtual_display_initial_size.trim();
        let (w, h) = text
            .split_once(['x', 'X'])
            .with_context(|| {
                format!("virtual_display_initial_size must be WIDTHxHEIGHT, got {text:?}")
            })?;
        // The floors are the ones the display is clamped to at creation, shared
        // rather than repeated: a size accepted here and then quietly changed
        // there would be a saved setting that does not describe the display.
        // Twice this is the pixel size, and the protocol carries pixels as u16 —
        // so anything past 32767 points cannot be described to the gateway at
        // all.
        let parse = |value: &str, axis: &str, min: u32| -> anyhow::Result<u32> {
            let n: u32 = value.trim().parse().with_context(|| {
                format!("virtual_display_initial_size {axis} must be a whole number, got {value:?}")
            })?;
            anyhow::ensure!(
                (min..=32_767).contains(&n),
                "virtual_display_initial_size {axis} must be between {min} and 32767 \
                 points, got {n}"
            );
            Ok(n)
        };
        Ok((
            parse(w, "width", crate::virtualdisplay::MIN_WIDTH_POINTS)?,
            parse(h, "height", crate::virtualdisplay::MIN_HEIGHT_POINTS)?,
        ))
    }

    /// The parsed listen address.
    ///
    /// A literal `address:port` and not a hostname, deliberately: the GUI can
    /// then reject a typo while the user is still looking at the field they
    /// typed it into, instead of the agent discovering it at bind time with
    /// nobody watching.
    pub fn socket_addr(&self) -> anyhow::Result<SocketAddr> {
        self.listen.parse().with_context(|| {
            format!(
                "listen must be an address:port such as 0.0.0.0:{}, got {:?}",
                rxa_proto::DEFAULT_PORT, self.listen
            )
        })
    }

    /// This Mac's 32-byte private key. Infallible after [`Config::validate`].
    pub fn private_key_bytes(&self) -> [u8; 32] {
        rxa_proto::key::parse_private(rxa_proto::key::Role::Agent, &self.private_key)
            .expect("private_key validated in Config::validate")
    }

    /// This Mac's public key, in the form the gateway's `agent_public_key`
    /// takes. Derived rather than stored, so it cannot go stale against the
    /// private key beside it.
    pub fn public_key(&self) -> String {
        rxa_proto::key::public_text_of(rxa_proto::key::Role::Agent, &self.private_key)
            .expect("private_key validated in Config::validate")
    }

    /// The gateway this agent answers, or `None` while it is unpaired.
    /// Infallible after [`Config::validate`].
    pub fn gateway_public_key_bytes(&self) -> Option<[u8; 32]> {
        if !self.is_paired() {
            return None;
        }
        Some(
            rxa_proto::key::parse_public(rxa_proto::key::Role::Gateway, &self.gateway_public_key)
                .expect("gateway_public_key validated in Config::validate"),
        )
    }

    /// Write this config to `path`, replacing whatever is there.
    ///
    /// Atomic by way of a temporary file and a rename, because the key lives in
    /// here: a crash midway through a plain rewrite would leave a truncated file
    /// and the agent with no credential at all at the next launch.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.validate()?;
        let name = path
            .file_name()
            .context("config path has no file name")?
            .to_string_lossy()
            .into_owned();
        // Beside the real file, so the rename stays within one filesystem.
        let temp = path.with_file_name(format!(".{name}.new"));
        write_private(&temp, &render(self))?;
        std::fs::rename(&temp, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
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

/// Replace this Mac's identity with an existing private key, keeping every other
/// setting.
///
/// The point is that an identity can outlive the machine it was made on: a Mac
/// that is re-imaged, restored from a backup, or replaced keeps the public key
/// every gateway already has, so nothing has to be re-paired. Without this the
/// only identity a Mac could have was one it minted itself, and losing the disk
/// meant editing every gateway that knew it.
///
/// Creates the config if there is none, so a freshly installed Mac can be given
/// its predecessor's identity before it has ever been launched — then discards
/// the key that creation minted, which is the one thing here that would
/// otherwise be a silent waste.
///
/// Returns the config as saved, so the caller can report the public key that
/// results — the value to check against the gateway.
pub fn import_private_key(explicit: Option<&Path>, private_key: &str) -> anyhow::Result<Config> {
    let private_key = private_key.trim();
    // Checked before anything is read or written, and by kind as well as
    // checksum: the neighbouring `rxap` and `rxgs` are exactly what a hurried
    // paste reaches for, and either would otherwise be written and then rejected
    // at the next launch.
    rxa_proto::key::parse_private(rxa_proto::key::Role::Agent, private_key)
        .map_err(|e| anyhow::anyhow!("not an agent private key: {e}"))?;

    let (mut config, path, _) = load_or_create(explicit)?;
    if config.private_key.trim() == private_key {
        // Not an error: re-running an import is how somebody makes sure it took.
        return Ok(config);
    }
    config.private_key = private_key.to_owned();
    config.save(&path)?;
    Ok(config)
}

/// Load the config, creating it with a freshly minted identity if it is absent.
///
/// There is no install script to write this file (the app registers itself — see
/// [`crate::loginitem`]), so first launch has to be self-sufficient: the user
/// drags the bundle in, opens it, and is left with two things to do — copy this
/// Mac's public key onto the gateway, and paste the gateway's back in. The
/// config it lands in is deliberately *unpaired* rather than half-written:
/// there is no gateway to name yet.
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
    let config = Config {
        listen: default_listen(),
        private_key: rxa_proto::key::generate_private(rxa_proto::key::Role::Agent),
        gateway_public_key: String::new(),
        virtual_display: false,
        virtual_display_initial_size: default_virtual_display_initial_size(),
    };
    config.save(&path)?;
    Ok((config, path, true))
}

/// Write a file readable only by its owner.
///
/// `private_key` in here is this Mac's whole identity — anything holding it can
/// be this Mac to the gateway — so the file is created 0600 from the start
/// rather than chmod'ed afterwards, which leaves no window where it is
/// world-readable.
fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// The file, rendered from a config.
///
/// Written whole every time (see the module docs), so this is the only place the
/// on-disk format exists.
fn render(config: &Config) -> String {
    format!(
        r#"# remotex-agent configuration.
#
# Managed from the menu bar item — Settings rewrites this file completely on
# every save. Hand edits to these comments will not survive the next one.
#
# Pairing is two keys, one each way, the way WireGuard pairs an interface with a
# peer. Neither is a shared secret, and only one of them is secret at all:
#
#   * private_key below is this Mac's identity. It never leaves this file — not
#     to the clipboard, not to a terminal, not into the gateway's config. Its
#     PUBLIC half is what the gateway needs, and Settings shows that one in full
#     with a Copy button; paste it as `agent_public_key` on the matching
#     [[targets]] entry in the gateway's remotex.toml.
#   * gateway_public_key is that gateway's public key, which `remotex rxa-pubkey`
#     prints. Until it is set this agent is unpaired: it listens, and refuses
#     every connection.

# Address to listen on, as address:port — 0.0.0.0 so the gateway host can reach
# it; narrow this to a specific interface if you prefer.
listen = "{listen}"

# Secret. Regenerating it means re-pairing: the gateway will refuse this Mac
# until its new public key is pasted there.
private_key = "{private_key}"

# The one gateway this Mac answers. Empty means unpaired. This is who may ask for
# a session; the agent serves one session at a time either way, and refuses a
# second until whoever is watching is taken over.
gateway_public_key = "{gateway_public_key}"

# There is no setting for which display to share: that is picked per session
# from the viewer or the browser, out of every display attached to this Mac.
#
# Give this Mac an extra display of the agent's own making — a private 2x desktop
# that no one is sitting in front of. It is an *addition*, not a replacement: the
# Mac's own screens stay shareable, and this one simply joins the list. It exists
# for as long as the agent runs, and it appears in System Settings > Displays
# like any other screen.
virtual_display = {virtual_display}

# The size that display is created at the FIRST time this Mac sees it, in points,
# WIDTHxHEIGHT, no smaller than 800x600 — and the largest mode macOS can ever
# render on it at 2x.
#
# Initial, not current. To change the size afterwards, use "Resize to window" in
# the browser or the viewer — on a gateway target with resize = true, it asks this
# display to match the window it is being viewed in, staying under the size above
# and keeping the density it is in.
#
# You can also resize it in System Settings > Displays, like any other screen, but
# that panel lists every size twice — HiDPI and "(low resolution)" — and a display
# shrunk much below this one drops out of HiDPI whichever entry is picked, so it
# comes back soft or oversized at a size nobody chose. macOS remembers whatever it
# ends up at, however it got there, and restores it at the next launch — which is
# also why editing this value will not move a display that has already been
# arranged.
#
# Its density needs no help either: whichever client is connected reports the
# screen it is on and this display matches it, 1x or 2x, on its own.
virtual_display_initial_size = "{virtual_display_initial_size}"
"#,
        listen = config.listen,
        private_key = config.private_key,
        gateway_public_key = config.gateway_public_key,
        virtual_display = config.virtual_display,
        virtual_display_initial_size = config.virtual_display_initial_size,
    )
}

/// A scratch directory that cleans itself up, so a failing assertion does not
/// leave the next run to trip over the leftovers.
///
/// Outside `mod tests` so [`crate::settings`]'s tests can use the same one —
/// they write configs (and generated keys) that need the same removal.
#[cfg(test)]
pub(crate) mod scratch {
    use std::path::{Path, PathBuf};

    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rxa-agent-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scratch::TempDir;
    use super::*;

    use rxa_proto::key::{self, Role};

    /// A config body with a valid identity, plus whatever `extra` adds. Left
    /// unpaired, since most of these tests are not about the gateway.
    fn with_identity(extra: &str) -> String {
        format!(
            "private_key = \"{}\"\n{extra}",
            key::generate_private(Role::Agent)
        )
    }

    /// A fresh gateway's public key, as `remotex rxa-pubkey` prints it.
    fn gateway_public_key() -> String {
        key::public_text_of(Role::Gateway, &key::generate_private(Role::Gateway)).unwrap()
    }

    fn sample() -> Config {
        Config {
            listen: default_listen(),
            private_key: key::generate_private(Role::Agent),
            gateway_public_key: gateway_public_key(),
            virtual_display: false,
            virtual_display_initial_size: default_virtual_display_initial_size(),
        }
    }

    #[test]
    fn minimal_config_listens_on_the_rxa_port_everywhere() {
        let config = Config::parse(&with_identity("")).unwrap();
        assert_eq!(config.listen, format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT));
        assert!(!config.virtual_display, "no extra display unless asked for");
    }

    // A config with no gateway named is the state every Mac starts in, and it
    // has to be a *valid* one: the agent must run before its public key can be
    // read out of the menu bar and taken to a gateway.
    #[test]
    fn an_agent_with_no_gateway_yet_is_unpaired_rather_than_invalid() {
        let config = Config::parse(&with_identity("")).unwrap();
        assert!(!config.is_paired());
        assert!(config.gateway_public_key_bytes().is_none());
        // And it still knows its own half, which is the thing to go and copy.
        assert!(config.public_key().starts_with("rxap"), "{}", config.public_key());
    }

    #[test]
    fn full_config_parses() {
        let private_key = key::generate_private(Role::Agent);
        let gateway = gateway_public_key();
        let config = Config::parse(&format!(
            "listen = \"192.168.1.5:9000\"\nprivate_key = \"{private_key}\"\n\
             gateway_public_key = \"{gateway}\"\nvirtual_display = true"
        ))
        .unwrap();
        assert_eq!(config.listen, "192.168.1.5:9000");
        assert!(config.virtual_display);
        assert!(config.is_paired());
        assert_eq!(
            config.private_key_bytes(),
            key::parse_private(Role::Agent, &private_key).unwrap()
        );
        assert_eq!(
            config.gateway_public_key_bytes(),
            Some(key::parse_public(Role::Gateway, &gateway).unwrap())
        );
        // Derived, not stored: the file holds one key and this is the other
        // face of it.
        assert_eq!(
            config.public_key(),
            key::public_text_of(Role::Agent, &private_key).unwrap()
        );
    }

    #[test]
    fn a_missing_or_malformed_private_key_is_rejected() {
        let err = Config::parse("listen = \"0.0.0.0:1\"").unwrap_err();
        assert!(format!("{err:#}").contains("private_key"), "{err:#}");

        let err = Config::parse("private_key = \"nonsense\"").unwrap_err();
        assert!(format!("{err:#}").contains("invalid private_key"), "{err:#}");

        // A single-character typo is caught by the key's own checksum.
        let mut chars: Vec<char> = key::generate_private(Role::Agent).chars().collect();
        chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        let err = Config::parse(&format!("private_key = \"{typo}\"")).unwrap_err();
        assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    }

    #[test]
    fn a_malformed_gateway_public_key_is_rejected() {
        let err = Config::parse(&with_identity("gateway_public_key = \"rxgpnope\"")).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid gateway_public_key"),
            "{err:#}"
        );
    }

    // The mistake the role in the prefix exists to catch, from this side: both
    // public keys are on screen while pairing, and pasting the wrong one here
    // would otherwise be a gateway that mysteriously never connects.
    #[test]
    fn this_macs_own_public_key_is_rejected_as_the_gateways() {
        let private_key = key::generate_private(Role::Agent);
        let own_public = key::public_text_of(Role::Agent, &private_key).unwrap();
        let err = Config::parse(&format!(
            "private_key = \"{private_key}\"\ngateway_public_key = \"{own_public}\""
        ))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("this is an agent public key"), "{msg}");
    }

    // The GUI validates a typed address with `validate`, so what it rejects is
    // exactly what the field refuses to save.
    #[test]
    fn a_listen_address_that_is_not_an_address_and_port_is_rejected() {
        for bad in ["", "0.0.0.0", "52381", "mac.local:52381", "0.0.0.0:notaport"] {
            let err = Config::parse(&with_identity(&format!("listen = {bad:?}"))).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("address:port"), "{bad:?} gave {msg}");
        }
    }

    #[test]
    fn a_valid_listen_address_parses_to_a_socket_address() {
        let config = Config::parse(&with_identity("listen = \"127.0.0.1:9000\"")).unwrap();
        let addr = config.socket_addr().unwrap();
        assert_eq!(addr.port(), 9000);
        assert!(addr.ip().is_loopback());
        // An IPv6 literal needs its brackets, and does not lose them.
        let config = Config::parse(&with_identity("listen = \"[::1]:9000\"")).unwrap();
        assert!(config.socket_addr().unwrap().is_ipv6());
    }

    #[test]
    fn typos_in_keys_are_rejected() {
        // deny_unknown_fields: a misspelled key is an error, not silence.
        let err = Config::parse(&with_identity("listn = \"x\"")).unwrap_err();
        assert!(format!("{err:#}").contains("listn"), "{err:#}");
    }

    // First launch has to be self-sufficient: there is no install script to
    // write this file.
    #[test]
    fn a_missing_config_is_created_with_a_fresh_identity_and_no_pairing() {
        let dir = TempDir::new("create");
        let path = dir.join("nested/config.toml");

        let (config, written, created) = load_or_create(Some(&path)).unwrap();
        assert!(created, "the first call should create the file");
        assert_eq!(written, path);
        assert!(path.exists());
        // A real, checksum-valid key, not a placeholder.
        assert_eq!(config.private_key.len(), rxa_proto::key::TEXT_LEN);
        key::parse_private(Role::Agent, &config.private_key).unwrap();
        assert_eq!(config.listen, format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT));
        // Unpaired: there is no gateway to name until someone brings one.
        assert!(!config.is_paired(), "a first launch has no gateway yet");

        // Owner-only: the private key in this file is this Mac's identity.
        assert_eq!(mode(&path), 0o600, "config must not be group/world readable");

        // A second call reuses it rather than minting a new identity —
        // otherwise every restart would silently break the pairing.
        let (again, _, created) = load_or_create(Some(&path)).unwrap();
        assert!(!created);
        assert_eq!(again.private_key, config.private_key);
    }

    // The point of importing: an identity outlives the Mac that made it, so a
    // re-imaged or replaced machine keeps the public key its gateways already
    // have and nothing is re-paired.
    #[test]
    fn importing_a_key_replaces_the_identity_and_keeps_every_other_setting() {
        let dir = TempDir::new("import");
        let path = dir.join("config.toml");
        let mut original = sample();
        original.listen = "127.0.0.1:9100".to_owned();
        original.virtual_display = true;
        original.virtual_display_initial_size = "1920x1200".to_owned();
        original.save(&path).unwrap();

        // The identity being restored, as another Mac (or a backup) holds it.
        let incoming = key::generate_private(Role::Agent);
        let imported = import_private_key(Some(&path), &incoming).unwrap();

        assert_eq!(imported.private_key, incoming);
        assert_eq!(
            imported.public_key(),
            key::public_text_of(Role::Agent, &incoming).unwrap(),
            "the public key must be the imported key's, which is what the gateway knows"
        );
        // Everything else survives: an import is not a reset.
        assert_eq!(imported.listen, "127.0.0.1:9100");
        assert!(imported.virtual_display);
        assert_eq!(imported.virtual_display_initial_size, "1920x1200");
        assert_eq!(imported.gateway_public_key, original.gateway_public_key);
        assert_eq!(load(Some(&path)).unwrap().0, imported, "not persisted");
        assert_eq!(mode(&path), 0o600, "the import must not widen the mode");
    }

    // Surrounding whitespace comes with any paste, and the key is trimmed rather
    // than stored as one the parser only just tolerates.
    #[test]
    fn an_imported_key_is_trimmed() {
        let dir = TempDir::new("import-trim");
        let path = dir.join("config.toml");
        sample().save(&path).unwrap();
        let incoming = key::generate_private(Role::Agent);
        let imported = import_private_key(Some(&path), &format!("  {incoming}\n")).unwrap();
        assert_eq!(imported.private_key, incoming);
    }

    // Re-running an import is how somebody checks it took, so it has to be a
    // no-op rather than an error.
    #[test]
    fn importing_the_key_already_in_place_is_not_an_error() {
        let dir = TempDir::new("import-same");
        let path = dir.join("config.toml");
        let original = sample();
        original.save(&path).unwrap();
        let again = import_private_key(Some(&path), &original.private_key).unwrap();
        assert_eq!(again, original);
    }

    // A Mac that has never been launched can still be given its predecessor's
    // identity — which is the case this exists for.
    #[test]
    fn importing_into_a_missing_config_creates_it_with_the_imported_key() {
        let dir = TempDir::new("import-fresh");
        let path = dir.join("nested/config.toml");
        let incoming = key::generate_private(Role::Agent);
        let imported = import_private_key(Some(&path), &incoming).unwrap();
        assert_eq!(imported.private_key, incoming);
        assert!(!imported.is_paired(), "still nothing to pair with");
        assert_eq!(load(Some(&path)).unwrap().0.private_key, incoming);
        assert_eq!(mode(&path), 0o600);
    }

    // Checked by kind as well as by checksum, and *before* anything is written:
    // the neighbouring `rxap` and `rxgs` are what a hurried paste reaches for,
    // and either written through would be a config rejected at the next launch.
    #[test]
    fn importing_the_wrong_kind_of_key_is_refused_and_changes_nothing() {
        let dir = TempDir::new("import-wrong");
        let path = dir.join("config.toml");
        let original = sample();
        original.save(&path).unwrap();

        let own_public = original.public_key();
        let gateway_private = key::generate_private(Role::Gateway);
        for (bad, expect) in [
            (own_public.as_str(), "an agent public key"),
            (gateway_private.as_str(), "a gateway private key"),
            ("rxasnonsense", "characters"),
            ("", "must start with"),
        ] {
            let err = import_private_key(Some(&path), bad).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("not an agent private key"), "{bad:?} gave {msg}");
            assert!(msg.contains(expect), "{bad:?} gave {msg}");
            assert_eq!(
                load(Some(&path)).unwrap().0,
                original,
                "{bad:?} must leave the file untouched"
            );
        }
    }

    // Saving is how every menu-bar edit lands, so a saved config has to read
    // back as itself — and keep the 0600 the key needs.
    #[test]
    fn saving_round_trips_and_stays_owner_only() {
        let dir = TempDir::new("save");
        let path = dir.join("config.toml");
        let mut config = sample();
        config.save(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(load(Some(&path)).unwrap().0, config);

        // Overwriting an existing file is the common case: every edit does it.
        config.listen = "127.0.0.1:9999".to_owned();
        config.virtual_display = true;
        config.private_key = key::generate_private(Role::Agent);
        config.gateway_public_key = gateway_public_key();
        config.save(&path).unwrap();
        assert_eq!(mode(&path), 0o600, "the rename must not widen the mode");
        assert_eq!(load(Some(&path)).unwrap().0, config);

        // And the temporary file it renamed from is not left lying about with a
        // copy of the key in it.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "config.toml")
            .collect();
        assert!(strays.is_empty(), "left behind {strays:?}");
    }

    // Nothing invalid should ever reach the disk: the file the agent reads at
    // the next launch is the only record of the key.
    #[test]
    fn saving_an_invalid_config_writes_nothing() {
        let dir = TempDir::new("invalid");
        let path = dir.join("config.toml");
        let good = sample();
        good.save(&path).unwrap();

        let bad = Config {
            listen: "not an address".to_owned(),
            ..good.clone()
        };
        assert!(bad.save(&path).is_err());
        assert_eq!(load(Some(&path)).unwrap().0, good, "the file must be untouched");
    }

    #[test]
    fn the_rendered_file_parses_back_to_the_same_config() {
        // The rendering is written before it is ever read, so a typo in it would
        // only surface on a user's next launch.
        let config = Config {
            listen: "10.0.0.1:1234".to_owned(),
            private_key: key::generate_private(Role::Agent),
            gateway_public_key: gateway_public_key(),
            virtual_display: true,
            virtual_display_initial_size: "1440x900".to_owned(),
        };
        assert_eq!(Config::parse(&render(&config)).unwrap(), config);

        // And the unpaired shape, which is what a first launch writes: an empty
        // `gateway_public_key` has to survive the round trip as empty rather
        // than becoming a line the parser then rejects.
        let unpaired = Config {
            gateway_public_key: String::new(),
            ..config
        };
        assert_eq!(Config::parse(&render(&unpaired)).unwrap(), unpaired);
    }

    // The floor the config accepts is the floor the display is created at. Were
    // they to drift, a size saved from the dialog would come back as a display of
    // some other size, with nothing having said so.
    //
    // Per axis, and 800x600 is the whole of it: a desktop is not square, so one
    // shared number would either let through a 600-point-wide display or refuse
    // an 800x600 one.
    #[test]
    fn a_virtual_display_initial_size_below_the_created_floor_is_rejected() {
        let (min_w, min_h) = (
            crate::virtualdisplay::MIN_WIDTH_POINTS,
            crate::virtualdisplay::MIN_HEIGHT_POINTS,
        );
        assert_eq!((min_w, min_h), (800, 600), "the documented minimum config");

        for (w, h, axis) in [(min_w - 1, min_h, "width"), (min_w, min_h - 1, "height")] {
            let err =
                Config::parse(&with_identity(&format!(
                    "virtual_display_initial_size = \"{w}x{h}\""
                )))
                .unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(axis), "{w}x{h} gave {msg}");
        }

        // And the floor itself is fine, so the message is a bound and not an
        // off-by-one.
        let config =
            Config::parse(&with_identity(&format!(
                "virtual_display_initial_size = \"{min_w}x{min_h}\""
            )))
            .unwrap();
        assert_eq!(
            config.virtual_display_initial_points().unwrap(),
            (min_w, min_h)
        );
    }

    #[test]
    fn the_default_config_path_is_per_user() {
        // The agent is a LaunchAgent, so its config belongs to the logged-in
        // user rather than a system-wide prefix.
        let path = default_path().unwrap();
        assert!(path.ends_with("Library/Application Support/remotex-agent/config.toml"), "{path:?}");
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}
