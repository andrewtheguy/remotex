//! The agent's TOML config, in the house style of
//! `packaging/etc/remotex.toml.example`.
//!
//! Deliberately tiny. The agent has no notion of users, targets or sessions —
//! it listens on one port, accepts one gateway at a time, and shares one
//! display. Everything else is the gateway's business.
//!
//! ## The file is written by the GUI, not by hand
//!
//! Every value in here is editable from the menu bar (see [`crate::menubar`]),
//! and [`crate::settings`] rewrites the file from scratch on each change. So the
//! file is a *rendering* of the config rather than a document with its own
//! identity: comments come from [`render`], and anything a user adds to it by
//! hand is lost the next time they change a setting from the menu. That is the
//! deal a GUI-owned config makes, and the header comment in the file says so.

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
    /// Pre-shared key, matching the gateway target's `psk`. This is the entire
    /// credential; without it no handshake completes.
    pub psk: String,
    /// Give the Mac an extra display, of the agent's own making.
    ///
    /// Not "share a display of our own instead of the Mac's screen", which is
    /// what this used to mean and was never what the API does:
    /// `CGVirtualDisplay` *adds* a monitor next to the ones already attached. So
    /// this only decides whether that display exists — which of them a session
    /// shares is chosen from the viewer or the browser, per session, and is no
    /// business of this file.
    ///
    /// Off by default, and still a setting rather than something a client can
    /// ask for: a display appearing and disappearing rearranges the windows on
    /// it, so it is created once at startup and lives as long as the agent.
    #[serde(default)]
    pub virtual_display: bool,
    /// The **initial** size of that display in points, as `WIDTHxHEIGHT`.
    ///
    /// Initial, not current, and the distinction is the whole of how this setting
    /// behaves. It is the size the display is created at the *first* time this Mac
    /// sees it. After that its resolution belongs to the Mac like any other
    /// screen's: it appears in System Settings > Displays, whoever is using the
    /// machine changes it there, and macOS remembers that choice against the
    /// display's identity and restores it on the next launch — so this value stops
    /// being what the display comes up at, and changing it will not move a display
    /// that has already been arranged. (Nothing here ever asks for a resolution
    /// twice; see [`crate::virtualdisplay`] for why, and
    /// `docs/known-issues.md` for the VM display stack that makes it unwise.)
    ///
    /// What it fixes forever is the *ceiling*: `maxPixels` and
    /// `sizeInMillimeters` are set from this at creation and cannot be changed,
    /// so this is the largest mode macOS can render on it at 2x, and every
    /// smaller size it offers has density to spare. Twice this many pixels get
    /// captured and encoded per frame, which is the reason not to ask for the
    /// largest display imaginable.
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
        rxa_proto::psk::parse(&self.psk).map_err(|e| anyhow::anyhow!("invalid psk: {e}"))?;
        self.socket_addr()?;
        self.virtual_display_initial_points()?;
        Ok(())
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
        let parse = |value: &str, axis: &str| -> anyhow::Result<u32> {
            let n: u32 = value.trim().parse().with_context(|| {
                format!("virtual_display_initial_size {axis} must be a whole number, got {value:?}")
            })?;
            // The floor is the one the display is clamped to at creation, shared
            // rather than repeated: a size accepted here and then quietly changed
            // there would be a saved setting that does not describe the display.
            // Twice this is the pixel size, and the protocol carries pixels as
            // u16 — so anything past 32767 points cannot be described to the
            // gateway at all.
            const MIN: u32 = crate::virtualdisplay::MIN_POINTS;
            anyhow::ensure!(
                (MIN..=32_767).contains(&n),
                "virtual_display_initial_size {axis} must be between {MIN} and 32767 \
                 points, got {n}"
            );
            Ok(n)
        };
        Ok((parse(w, "width")?, parse(h, "height")?))
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

    /// The 32-byte key. Infallible after [`Config::validate`].
    pub fn psk_bytes(&self) -> [u8; 32] {
        rxa_proto::psk::parse(&self.psk).expect("psk validated in Config::validate")
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

/// Load the config, creating it with a freshly generated key if it is absent.
///
/// There is no install script to write this file (the app registers itself — see
/// [`crate::loginitem`]), so first launch has to be self-sufficient: the user
/// drags the bundle in, opens it, and the only thing left to do is copy the key
/// from the menu bar onto the gateway.
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
        psk: rxa_proto::psk::generate(),
        virtual_display: false,
        virtual_display_initial_size: default_virtual_display_initial_size(),
    };
    config.save(&path)?;
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
# Managed from the menu bar item — Settings and Pre-Shared Key both write this
# file, and each write rewrites it completely. Hand edits to these comments will
# not survive the next one.
#
# The psk below is the entire credential: the gateway authenticates with it and
# nothing else, which is what makes a reconnect need no login. The *same* key
# must appear as `psk` on the matching [[targets]] entry in the gateway's
# remotex.toml. Read it from the menu bar: Pre-Shared Key.

# Address to listen on, as address:port — 0.0.0.0 so the gateway host can reach
# it; narrow this to a specific interface if you prefer.
listen = "{listen}"

psk = "{psk}"

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
# WIDTHxHEIGHT — and the largest mode macOS can ever render on it at 2x.
#
# Initial, not current. After the first launch its resolution belongs to the Mac
# like any other screen's: change it in System Settings > Displays, and macOS
# remembers that against the display and restores it next time. Editing this
# value will not move a display that has already been arranged — only a size
# smaller than the one in use is worth changing here, and only for the ceiling.
virtual_display_initial_size = "{virtual_display_initial_size}"
"#,
        listen = config.listen,
        psk = config.psk,
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

    fn with_psk(extra: &str) -> String {
        format!("psk = \"{}\"\n{extra}", rxa_proto::psk::generate())
    }

    fn sample() -> Config {
        Config {
            listen: default_listen(),
            psk: rxa_proto::psk::generate(),
            virtual_display: false,
            virtual_display_initial_size: default_virtual_display_initial_size(),
        }
    }

    #[test]
    fn minimal_config_listens_on_the_rxa_port_everywhere() {
        let config = Config::parse(&with_psk("")).unwrap();
        assert_eq!(config.listen, format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT));
        assert!(!config.virtual_display, "no extra display unless asked for");
    }

    #[test]
    fn full_config_parses() {
        let psk = rxa_proto::psk::generate();
        let config =
            Config::parse(&format!(
                "listen = \"192.168.1.5:9000\"\npsk = \"{psk}\"\nvirtual_display = true"
            ))
            .unwrap();
        assert_eq!(config.listen, "192.168.1.5:9000");
        assert!(config.virtual_display);
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

    // The GUI validates a typed address with `validate`, so what it rejects is
    // exactly what the field refuses to save.
    #[test]
    fn a_listen_address_that_is_not_an_address_and_port_is_rejected() {
        for bad in ["", "0.0.0.0", "52381", "mac.local:52381", "0.0.0.0:notaport"] {
            let err = Config::parse(&with_psk(&format!("listen = {bad:?}"))).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("address:port"), "{bad:?} gave {msg}");
        }
    }

    #[test]
    fn a_valid_listen_address_parses_to_a_socket_address() {
        let config = Config::parse(&with_psk("listen = \"127.0.0.1:9000\"")).unwrap();
        let addr = config.socket_addr().unwrap();
        assert_eq!(addr.port(), 9000);
        assert!(addr.ip().is_loopback());
        // An IPv6 literal needs its brackets, and does not lose them.
        let config = Config::parse(&with_psk("listen = \"[::1]:9000\"")).unwrap();
        assert!(config.socket_addr().unwrap().is_ipv6());
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
        let dir = TempDir::new("create");
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
        assert_eq!(mode(&path), 0o600, "config must not be group/world readable");

        // A second call reuses it rather than minting a new key — otherwise
        // every restart would silently break the pairing with the gateway.
        let (again, _, created) = load_or_create(Some(&path)).unwrap();
        assert!(!created);
        assert_eq!(again.psk, config.psk);
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
        config.psk = rxa_proto::psk::generate();
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
            psk: rxa_proto::psk::generate(),
            virtual_display: true,
            virtual_display_initial_size: "1440x900".to_owned(),
        };
        assert_eq!(Config::parse(&render(&config)).unwrap(), config);
    }

    // The floor the config accepts is the floor the display is created at. Were
    // they to drift, a size saved from the dialog would come back as a display of
    // some other size, with nothing having said so.
    #[test]
    fn a_virtual_display_initial_size_below_the_created_floor_is_rejected() {
        let floor = crate::virtualdisplay::MIN_POINTS;
        let err = Config::parse(&with_psk(&format!(
            "virtual_display_initial_size = \"{}x{floor}\"",
            floor - 1
        )))
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("width"), "{msg}");
        assert!(msg.contains(&floor.to_string()), "{msg}");

        // And the floor itself is fine, so the message is a bound and not an
        // off-by-one.
        let config =
            Config::parse(&with_psk(&format!(
                "virtual_display_initial_size = \"{floor}x{floor}\""
            )))
            .unwrap();
        assert_eq!(config.virtual_display_initial_points().unwrap(), (floor, floor));
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
