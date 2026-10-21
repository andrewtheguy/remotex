//! The master password in the account's credential store.
//!
//! One entry per OS account — service `remotex`, account `master` — opens every
//! config that account keeps (see [`crate::vault`]): the Keychain on macOS,
//! Credential Manager on Windows, the Secret Service (GNOME Keyring, KWallet) on
//! Linux. A build without the `keychain` feature — a container's — has none, and
//! says so when asked.
//!
//! A store can be there and still not answer: Credential Manager refuses a network
//! logon (ssh) with `ERROR_NO_SUCH_LOGON_SESSION`, a locked macOS keychain refuses
//! a session that cannot show its unlock dialog, and a headless Linux has no Secret
//! Service at all. [`load`] tells that apart from an empty store, so whoever asked
//! can say why and what to do instead.

use crate::vault::MasterPassword;

#[cfg(feature = "keychain")]
const SERVICE: &str = "remotex";
#[cfg(feature = "keychain")]
const ACCOUNT: &str = "master";

/// The master password the store keeps: `Ok(None)` when it keeps none, an error
/// when there is no store to ask or it would not answer.
pub fn load() -> anyhow::Result<Option<MasterPassword>> {
    #[cfg(feature = "keychain")]
    {
        use anyhow::Context as _;
        let entry = entry()?;
        match entry.get_password() {
            Ok(password) => MasterPassword::new(password)
                .map(Some)
                .context("the keychain's remotex master password is empty"),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("cannot read the keychain"),
        }
    }
    #[cfg(not(feature = "keychain"))]
    {
        anyhow::bail!("this build has no keychain")
    }
}

/// Keep `master` as the account's master password, replacing any kept before.
pub fn save(master: &MasterPassword) -> anyhow::Result<()> {
    #[cfg(feature = "keychain")]
    {
        use anyhow::Context as _;
        entry()?
            .set_password(master.as_str())
            .context("cannot write the keychain")
    }
    #[cfg(not(feature = "keychain"))]
    {
        let _ = master;
        anyhow::bail!("this build has no keychain")
    }
}

#[cfg(feature = "keychain")]
fn entry() -> anyhow::Result<keyring_core::Entry> {
    use anyhow::Context as _;
    init_store()?;
    keyring_core::Entry::new(SERVICE, ACCOUNT).context("cannot name the keychain entry")
}

/// Install the store once per process.
///
/// Under test it is keyring-core's in-memory mock, never the machine's own store:
/// a locked keychain or Secret Service answers a write with an unlock dialog, which
/// hangs an unattended run, and a test has no business in the developer's store.
#[cfg(feature = "keychain")]
fn init_store() -> anyhow::Result<()> {
    static INSTALLED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            #[cfg(test)]
            {
                keyring_core::set_default_store(
                    keyring_core::mock::Store::new().map_err(|error| error.to_string())?,
                );
                Ok(())
            }
            #[cfg(not(test))]
            {
                native_store()
            }
        })
        .clone()
        .map_err(|error| anyhow::anyhow!("no keychain: {error}"))
}

#[cfg(feature = "keychain")]
fn native_store() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(target_os = "windows")]
    let store = windows_native_keyring_store::Store::new();
    #[cfg(target_os = "linux")]
    let store = zbus_secret_service_keyring_store::Store::new();
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    return Err("this platform has no keychain this build speaks".to_owned());
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        keyring_core::set_default_store(store.map_err(|error| error.to_string())?);
        Ok(())
    }
}

#[cfg(all(test, feature = "keychain"))]
mod tests {
    use super::*;

    /// Round-trips a throwaway entry through the machine's real store, under an
    /// account of its own so the master password kept there is never touched.
    ///
    /// Ignored because it writes the real store, and because only a desktop
    /// session's store answers: run it there (macsandbox on a Mac, a logged-on
    /// account on Windows, a Linux desktop with GNOME Keyring or KWallet).
    #[test]
    #[ignore = "writes the machine's real keychain; run from a desktop session"]
    fn the_real_store_round_trips_a_throwaway_entry() {
        /// Deletes the entry however the test ends.
        struct Cleanup(keyring_core::Entry);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.delete_credential();
            }
        }

        // Not `init_store`: under test that is the mock, and this is the real one.
        native_store().expect("a native store");
        let vendor = keyring_core::get_default_store().unwrap().vendor();
        assert!(!vendor.contains("Mock"), "{vendor}");
        let account = format!("test-{}", std::process::id());
        let entry = keyring_core::Entry::new(SERVICE, &account).unwrap();
        entry.set_password("correct horse battery staple").expect("write the real store");
        let cleanup = Cleanup(entry);
        assert_eq!(cleanup.0.get_password().unwrap(), "correct horse battery staple");
        cleanup.0.delete_credential().expect("delete from the real store");
        assert!(matches!(cleanup.0.get_password(), Err(keyring_core::Error::NoEntry)));
        eprintln!("round-tripped through {vendor}");
    }

    /// The guard for every test in this binary: the store a test reaches is the
    /// mock, and a round trip through it works.
    #[test]
    fn tests_reach_the_mock_store_and_round_trip() {
        init_store().unwrap();
        let vendor = keyring_core::get_default_store().unwrap().vendor();
        assert!(vendor.contains("Mock"), "tests must not reach a real store, got {vendor}");
        save(&MasterPassword::new("in memory only".to_owned()).unwrap()).unwrap();
        assert_eq!(load().unwrap().unwrap().as_str(), "in memory only");
    }
}
