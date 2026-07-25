//! The config as the GUI sees it: what the agent is running, and what the file
//! now says.
//!
//! Every setting the agent has is editable from the menu bar (see
//! [`crate::menubar`]), and an edit does exactly two things here: validate, and
//! write the file. **Nothing in this module touches the running agent.** What
//! applies a change is a restart, which the menu does by re-execing the process
//! as soon as a save changes anything (see [`crate::restart`]). Rebinding a
//! listener under a live connection, swapping the key the current gateway
//! authenticated with, restarting the capture stream on another display — each is
//! a small pile of machinery, and the re-exec is one line that covers all three
//! and cannot leave them half-applied.
//!
//! [`running`] is the config this process was actually launched with, kept beside
//! the saved one for the case where the restart does *not* happen: a re-exec that
//! failed, or a file somebody edited by hand. Then the menu can say "restart to
//! apply" rather than showing a setting that is not in force.
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

    /// Replace the config with what the settings dialog collected.
    ///
    /// The whole config at once, because the dialog edits it that way: one panel,
    /// one Save, one write. An error means nothing was written — the file and
    /// everything this struct reports are exactly as they were, and the caller can
    /// put the dialog back up on the same values.
    ///
    /// Note what this does *not* do: apply anything. Returns whether the file
    /// changed, which is the caller's cue to restart into it.
    pub fn apply(&self, next: Config) -> anyhow::Result<bool> {
        // Whitespace round a pasted value is the user's typing, not their intent.
        let next = Config {
            listen: next.listen.trim().to_owned(),
            psk: next.psk.trim().to_owned(),
            display: next.display,
        };
        let mut saved = self.saved.lock().unwrap();
        if next == *saved {
            return Ok(false);
        }
        // Validates before it writes, so an invalid edit changes neither the file
        // nor what this struct reports.
        next.save(&self.path)?;
        info!("settings: saved {}", self.path.display());
        *saved = next;
        Ok(true)
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
    fn a_saved_edit_reaches_the_file_and_asks_to_be_restarted_into() {
        let (settings, path) = settings("edit");
        let mut next = settings.saved();
        next.listen = "127.0.0.1:9001".to_owned();
        next.display = 2;

        assert!(settings.apply(next.clone()).unwrap(), "a change was saved");
        assert_eq!(settings.saved(), next);
        assert_eq!(on_disk(&path), next, "not persisted");
        assert!(settings.restart_pending());
        // The running agent is untouched until it restarts, and the menu bar has
        // to be able to say so.
        assert_eq!(
            settings.running().listen,
            format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT)
        );
        assert_eq!(settings.running().display, 0);
    }

    // The dialog hands back all three values whether or not they were touched, so
    // pressing Save on an unchanged dialog must not report a change — that is what
    // would otherwise restart the agent for nothing.
    #[test]
    fn saving_an_unchanged_config_is_not_a_change() {
        let (settings, _) = settings("unchanged");
        assert!(!settings.apply(settings.saved()).unwrap());
        assert!(!settings.restart_pending());
    }

    // A key pasted into the dialog is stored verbatim, and the old one keeps
    // authenticating until the restart.
    #[test]
    fn a_new_key_is_saved_without_becoming_the_running_one() {
        let (settings, path) = settings("psk");
        let before = settings.saved().psk;
        let after = rxa_proto::psk::generate();
        let mut next = settings.saved();
        next.psk = after.clone();

        assert!(settings.apply(next).unwrap());
        assert_eq!(settings.saved().psk, after);
        assert_eq!(on_disk(&path).psk, after);
        assert_eq!(settings.running().psk, before);
        assert!(settings.restart_pending());
    }

    // An edit the config layer rejects must leave no trace: not in the file, and
    // not in what the GUI reports back. The dialog reopens on the typed values,
    // so nothing here may be half-kept.
    #[test]
    fn a_rejected_edit_changes_nothing() {
        let (settings, path) = settings("reject");
        let before = settings.saved();

        let mut bad = before.clone();
        bad.listen = "port 52381".to_owned();
        let err = settings.apply(bad).unwrap_err();
        assert!(format!("{err:#}").contains("address:port"), "{err:#}");

        let mut bad = before.clone();
        bad.psk = "rxanonsense".to_owned();
        let err = settings.apply(bad).unwrap_err();
        assert!(format!("{err:#}").contains("psk"), "{err:#}");

        assert_eq!(settings.saved(), before);
        assert_eq!(on_disk(&path), before);
        assert!(!settings.restart_pending());
    }

    // Reverting an edit closes the gap again — a restart is only pending while
    // the file and the running agent actually disagree.
    #[test]
    fn putting_a_value_back_clears_the_pending_restart() {
        let (settings, _) = settings("revert");
        let original = settings.saved();
        let mut next = original.clone();
        next.listen = "127.0.0.1:9003".to_owned();
        settings.apply(next).unwrap();
        assert!(settings.restart_pending());
        settings.apply(original).unwrap();
        assert!(!settings.restart_pending());
    }

    // Whitespace round pasted values is the user's typing, not their intent — and
    // an untrimmed key would be stored as one the config parser barely accepts.
    #[test]
    fn pasted_values_are_trimmed() {
        let (settings, _) = settings("trim");
        let psk = rxa_proto::psk::generate();
        let next = Config {
            listen: "  127.0.0.1:9002\n".to_owned(),
            psk: format!(" {psk}\n"),
            display: 0,
        };
        assert!(settings.apply(next).unwrap());
        assert_eq!(settings.saved().listen, "127.0.0.1:9002");
        assert_eq!(settings.saved().psk, psk);
    }
}
