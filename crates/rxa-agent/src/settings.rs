//! The config as the GUI sees it: what the agent is running, and what the file
//! now says.
//!
//! Every setting the agent has is editable from the menu bar (see
//! [`crate::menubar`]), and an edit does exactly two things: validate, and write
//! the file. **Nothing is applied to the running agent.** Rebinding a listener
//! under a live connection, swapping the key the current gateway authenticated
//! with, restarting the capture stream on another display — each is a small pile
//! of machinery, and all of it exists to save a background agent one restart.
//!
//! So the deal is the plain one: a change takes effect the next time the agent
//! starts. What that costs is honesty about the gap, which is what [`running`]
//! is for — the config this process was launched with, kept beside the saved one
//! so the menu can say "restart to apply" instead of quietly showing a setting
//! that is not in force.
//!
//! [`running`]: Settings::running

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use log::info;

use crate::config::Config;

pub struct Settings {
    path: PathBuf,
    /// What this process is serving. Fixed for its lifetime — the network thread
    /// was handed these values at startup and never looks again.
    running: Config,
    /// What the file says, which is what the next launch will serve.
    saved: Mutex<Config>,
}

impl Settings {
    pub fn new(config: Config, path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            running: config.clone(),
            saved: Mutex::new(config),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The config the running agent is actually serving.
    pub fn running(&self) -> &Config {
        &self.running
    }

    /// The config in the file, which the next launch will serve.
    pub fn saved(&self) -> Config {
        self.saved.lock().unwrap().clone()
    }

    /// Whether anything has been changed that the running agent is not obeying.
    pub fn restart_pending(&self) -> bool {
        *self.saved.lock().unwrap() != self.running
    }

    /// Change the address the agent listens on, from the next launch.
    pub fn set_listen(&self, listen: &str) -> anyhow::Result<()> {
        let listen = listen.trim().to_owned();
        self.update(|config| config.listen = listen)
    }

    /// Change which display is shared, by index into the shareable-display list.
    pub fn set_display(&self, display: usize) -> anyhow::Result<()> {
        self.update(|config| config.display = display)
    }

    /// Mint a new pre-shared key, returning it so the GUI can offer it for
    /// copying — it is the one value the user has to carry somewhere else.
    ///
    /// The *running* agent keeps accepting the old key until it restarts, which
    /// the caller has to say out loud: a regenerated credential that is not in
    /// force yet is the kind of thing someone acts on immediately.
    pub fn regenerate_psk(&self) -> anyhow::Result<String> {
        let psk = rxa_proto::psk::generate();
        self.update(|config| config.psk = psk.clone())?;
        Ok(psk)
    }

    /// Validate, then write. Nothing else — see the module docs.
    fn update(&self, edit: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
        let mut saved = self.saved.lock().unwrap();
        let mut next = saved.clone();
        edit(&mut next);
        if next == *saved {
            return Ok(());
        }
        // Validates before it writes, so an invalid edit changes neither the file
        // nor what this struct reports.
        next.save(&self.path)?;
        info!("settings: saved {}", self.path.display());
        *saved = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rxa-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn settings(tag: &str) -> (Arc<Settings>, PathBuf) {
        let path = scratch(tag);
        let config = Config {
            listen: format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT),
            psk: rxa_proto::psk::generate(),
            display: 0,
        };
        config.save(&path).unwrap();
        (Settings::new(config, path.clone()), path)
    }

    fn on_disk(path: &Path) -> Config {
        crate::config::load(Some(path)).unwrap().0
    }

    #[test]
    fn a_fresh_agent_is_running_what_the_file_says() {
        let (settings, path) = settings("fresh");
        assert!(!settings.restart_pending());
        assert_eq!(settings.saved(), on_disk(&path));
        assert_eq!(&settings.saved(), settings.running());
    }

    #[test]
    fn an_edit_reaches_the_file_and_needs_a_restart() {
        let (settings, path) = settings("edit");
        settings.set_listen("127.0.0.1:9001").unwrap();

        assert_eq!(settings.saved().listen, "127.0.0.1:9001");
        assert_eq!(on_disk(&path).listen, "127.0.0.1:9001", "not persisted");
        assert!(settings.restart_pending());
        // The running agent is untouched: it is still on the old port, and the
        // menu bar has to be able to say so.
        assert_eq!(
            settings.running().listen,
            format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT)
        );

        settings.set_display(2).unwrap();
        assert_eq!(on_disk(&path).display, 2);
        assert_eq!(settings.running().display, 0);
    }

    #[test]
    fn a_regenerated_key_is_new_valid_and_saved() {
        let (settings, path) = settings("psk");
        let before = settings.saved().psk;
        let after = settings.regenerate_psk().unwrap();

        assert_ne!(before, after);
        rxa_proto::psk::parse(&after).unwrap();
        assert_eq!(settings.saved().psk, after);
        assert_eq!(on_disk(&path).psk, after);
        // The old key is still the one that authenticates, until a restart.
        assert_eq!(settings.running().psk, before);
        assert!(settings.restart_pending());
    }

    // An edit the config layer rejects must leave no trace: not in the file, and
    // not in what the GUI reports back.
    #[test]
    fn a_rejected_edit_changes_nothing() {
        let (settings, path) = settings("reject");
        let before = settings.saved();

        let err = settings.set_listen("port 52381").unwrap_err();
        assert!(format!("{err:#}").contains("address:port"), "{err:#}");

        assert_eq!(settings.saved(), before);
        assert_eq!(on_disk(&path), before);
        assert!(!settings.restart_pending());
    }

    // Reverting an edit closes the gap again — a restart is only pending while
    // the file and the running agent actually disagree.
    #[test]
    fn putting_a_value_back_clears_the_pending_restart() {
        let (settings, _) = settings("revert");
        let original = settings.running().listen.clone();
        settings.set_listen("127.0.0.1:9003").unwrap();
        assert!(settings.restart_pending());
        settings.set_listen(&original).unwrap();
        assert!(!settings.restart_pending());
    }

    // Whitespace round a pasted address is the user's typing, not their intent.
    #[test]
    fn a_pasted_address_is_trimmed() {
        let (settings, _) = settings("trim");
        settings.set_listen("  127.0.0.1:9002\n").unwrap();
        assert_eq!(settings.saved().listen, "127.0.0.1:9002");
    }
}
