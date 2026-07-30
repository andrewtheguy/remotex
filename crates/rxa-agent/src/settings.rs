//! The settings as the GUI sees them: what the agent is running, and what the
//! files now say.
//!
//! Two files, because they are two different kinds of thing — the config the menu
//! bar rewrites whole, and the authorized list a person annotates (see
//! [`crate::authorized`]) — and one pair of `running`/`saved`, because they are
//! read at the same moment and applied by the same restart.
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

use crate::authorized::Authorized;
use crate::config::Config;

pub struct Settings {
    path: PathBuf,
    authorized_path: PathBuf,
    /// What this process is serving. Fixed for its lifetime — the network thread
    /// was handed these values at startup and never looks again.
    running: Config,
    running_authorized: Authorized,
    /// What the files say, which is what the next launch will serve.
    saved: Mutex<Config>,
    saved_authorized: Mutex<Authorized>,
}

impl Settings {
    pub fn new(
        config: Config,
        path: PathBuf,
        authorized: Authorized,
        authorized_path: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            path,
            authorized_path,
            running: config.clone(),
            running_authorized: authorized.clone(),
            saved: Mutex::new(config),
            saved_authorized: Mutex::new(authorized),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn authorized_path(&self) -> &Path {
        &self.authorized_path
    }

    /// The config the running agent is actually serving.
    pub fn running(&self) -> &Config {
        &self.running
    }

    /// The list the running agent is actually judging dials against.
    pub fn running_authorized(&self) -> &Authorized {
        &self.running_authorized
    }

    /// The config in the file, which the next launch will serve.
    pub fn saved(&self) -> Config {
        self.saved.lock().unwrap().clone()
    }

    /// The list in the file, which the next launch will judge dials against.
    pub fn saved_authorized(&self) -> Authorized {
        self.saved_authorized.lock().unwrap().clone()
    }

    /// Whether anything has been changed that the running agent is not obeying.
    pub fn restart_pending(&self) -> bool {
        *self.saved.lock().unwrap() != self.running
            || *self.saved_authorized.lock().unwrap() != self.running_authorized
    }

    /// Replace the config and the authorized list with what the settings dialog
    /// collected.
    ///
    /// Both at once, because the dialog edits them that way: one panel, one Save.
    /// An error means **nothing** was written — including the half that was fine,
    /// since two files half-applied is a state no restart resolves — and everything
    /// this struct reports is exactly as it was, so the caller can put the dialog
    /// back up on the same values.
    ///
    /// Note what this does *not* do: apply anything. Returns whether either file
    /// changed, which is the caller's cue to restart into them.
    pub fn apply(&self, next: Config, next_authorized: Authorized) -> anyhow::Result<bool> {
        // Whitespace round a pasted value is the user's typing, not their intent.
        // The list needs no equivalent: `Authorized::parse` normalized it, and the
        // whitespace *inside* it is a person's layout.
        let next = Config {
            listen: next.listen.trim().to_owned(),
            private_key: next.private_key.trim().to_owned(),
            virtual_display: next.virtual_display,
            virtual_display_initial_size: next.virtual_display_initial_size.trim().to_owned(),
        };
        let mut saved = self.saved.lock().unwrap();
        let mut saved_authorized = self.saved_authorized.lock().unwrap();
        let (config_changed, list_changed) =
            (next != *saved, next_authorized != *saved_authorized);
        if !config_changed && !list_changed {
            return Ok(false);
        }
        // Validated before either write, so one file being rejected cannot leave
        // the other one already replaced.
        next.validate()?;
        if config_changed {
            next.save(&self.path)?;
            info!("settings: saved {}", self.path.display());
        }
        if list_changed {
            next_authorized.save(&self.authorized_path)?;
            info!(
                "settings: saved {} ({} authorized gateways)",
                self.authorized_path.display(),
                next_authorized.len()
            );
        }
        *saved = next;
        *saved_authorized = next_authorized;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorized;
    use crate::config::scratch::TempDir;
    use rxa_proto::key::{self, Role};

    /// A fresh gateway's public key, as `remotex rxa-pubkey` prints it.
    fn gateway_public_key() -> String {
        key::public_text_of(Role::Gateway, &key::generate_private(Role::Gateway)).unwrap()
    }

    fn list(text: &str) -> Authorized {
        Authorized::parse(text).unwrap()
    }

    /// The `TempDir` comes back with the settings: dropping it removes the
    /// directory, the config written into it and the key that config carries, so
    /// it has to outlive the test rather than the helper.
    fn settings(tag: &str) -> (Arc<Settings>, PathBuf, TempDir) {
        let dir = TempDir::new(&format!("settings-{tag}"));
        let path = dir.join("config.toml");
        let config = Config {
            listen: format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT),
            private_key: key::generate_private(Role::Agent),
            virtual_display: false,
            virtual_display_initial_size: "1600x1000".to_owned(),
        };
        config.save(&path).unwrap();
        let authorized_path = authorized::path_beside(&path);
        let authorized = list(&format!("{} home server\n", gateway_public_key()));
        authorized.save(&authorized_path).unwrap();
        (
            Settings::new(config, path.clone(), authorized, authorized_path),
            path,
            dir,
        )
    }

    fn on_disk(path: &Path) -> Config {
        crate::config::load(Some(path)).unwrap().0
    }

    fn list_on_disk(settings: &Settings) -> Authorized {
        Authorized::load(settings.authorized_path()).unwrap()
    }

    #[test]
    fn a_fresh_agent_is_running_what_the_files_say() {
        let (settings, path, _dir) = settings("fresh");
        assert!(!settings.restart_pending());
        assert_eq!(settings.saved(), on_disk(&path));
        assert_eq!(&settings.saved(), settings.running());
        assert_eq!(settings.saved_authorized(), list_on_disk(&settings));
        assert_eq!(&settings.saved_authorized(), settings.running_authorized());
        // Beside the config, so one --config moves the whole of an agent's state.
        assert_eq!(settings.authorized_path().parent(), path.parent());
    }

    #[test]
    fn a_saved_edit_reaches_the_file_and_asks_to_be_restarted_into() {
        let (settings, path, _dir) = settings("edit");
        let mut next = settings.saved();
        next.listen = "127.0.0.1:9001".to_owned();
        next.virtual_display = true;

        assert!(
            settings.apply(next.clone(), settings.saved_authorized()).unwrap(),
            "a change was saved"
        );
        assert_eq!(settings.saved(), next);
        assert_eq!(on_disk(&path), next, "not persisted");
        assert!(settings.restart_pending());
        // The running agent is untouched until it restarts, and the menu bar has
        // to be able to say so.
        assert_eq!(
            settings.running().listen,
            format!("0.0.0.0:{}", rxa_proto::DEFAULT_PORT)
        );
        assert!(!settings.running().virtual_display);
    }

    // The dialog hands back all three values whether or not they were touched, so
    // pressing Save on an unchanged dialog must not report a change — that is what
    // would otherwise restart the agent for nothing.
    #[test]
    fn saving_an_unchanged_config_is_not_a_change() {
        let (settings, _, _dir) = settings("unchanged");
        assert!(
            !settings
                .apply(settings.saved(), settings.saved_authorized())
                .unwrap()
        );
        assert!(!settings.restart_pending());
    }

    // A gateway added to the list is saved verbatim, and the old list keeps
    // deciding who gets in until the restart.
    #[test]
    fn a_new_authorized_gateway_is_saved_without_becoming_the_running_list() {
        let (settings, _, _dir) = settings("gateway");
        let before = settings.saved_authorized();
        let after = list(&format!(
            "{}{} the laptop\n",
            before.text(),
            gateway_public_key()
        ));
        assert_eq!(after.len(), 2);

        assert!(settings.apply(settings.saved(), after.clone()).unwrap());
        assert_eq!(settings.saved_authorized(), after);
        assert_eq!(list_on_disk(&settings), after);
        assert_eq!(settings.running_authorized(), &before);
        assert!(settings.restart_pending());
    }

    // Commenting a line out is how an entry is parked, so it has to be a change
    // even though no key was added or removed: the running agent still answers
    // that gateway until it restarts.
    #[test]
    fn commenting_an_entry_out_is_a_change_worth_restarting_for() {
        let (settings, _, _dir) = settings("parked");
        let parked = list(&format!("# away for now:\n#{}", settings.saved_authorized().text().trim()));
        assert!(parked.is_empty());

        assert!(settings.apply(settings.saved(), parked.clone()).unwrap());
        assert_eq!(list_on_disk(&settings), parked);
        assert!(settings.restart_pending());
        assert_eq!(settings.running_authorized().len(), 1, "still answering it");
    }

    // Regenerating this Mac's identity is the same deal, and the more important
    // half of it: the gateway keeps talking to the old key until the re-exec,
    // so the menu has to be able to say a restart is pending.
    #[test]
    fn a_regenerated_identity_is_saved_without_becoming_the_running_one() {
        let (settings, path, _dir) = settings("identity");
        let before = settings.saved().private_key;
        let after = key::generate_private(Role::Agent);
        let mut next = settings.saved();
        next.private_key = after.clone();

        assert!(settings.apply(next, settings.saved_authorized()).unwrap());
        assert_eq!(on_disk(&path).private_key, after);
        assert_eq!(settings.running().private_key, before);
        assert!(settings.restart_pending());
    }

    // An edit the config layer rejects must leave no trace: not in the file, and
    // not in what the GUI reports back. The dialog reopens on the typed values,
    // so nothing here may be half-kept.
    #[test]
    fn a_rejected_edit_changes_nothing() {
        let (settings, path, _dir) = settings("reject");
        let before = settings.saved();

        let before_list = settings.saved_authorized();

        let mut bad = before.clone();
        bad.listen = "port 52381".to_owned();
        let err = settings
            .apply(bad, settings.saved_authorized())
            .unwrap_err();
        assert!(format!("{err:#}").contains("address:port"), "{err:#}");

        // And the rejected half must not leave the *other* file already replaced:
        // a good list beside a bad address writes neither.
        let mut bad = before.clone();
        bad.listen = "port 52381".to_owned();
        let good_list = list(&format!("{} somewhere else\n", gateway_public_key()));
        assert!(settings.apply(bad, good_list).is_err());

        assert_eq!(settings.saved(), before);
        assert_eq!(on_disk(&path), before);
        assert_eq!(settings.saved_authorized(), before_list);
        assert_eq!(list_on_disk(&settings), before_list);
        assert!(!settings.restart_pending());
    }

    // Reverting an edit closes the gap again — a restart is only pending while
    // the file and the running agent actually disagree.
    #[test]
    fn putting_a_value_back_clears_the_pending_restart() {
        let (settings, _, _dir) = settings("revert");
        let original = settings.saved();
        let mut next = original.clone();
        next.listen = "127.0.0.1:9003".to_owned();
        settings.apply(next, settings.saved_authorized()).unwrap();
        assert!(settings.restart_pending());
        settings.apply(original, settings.saved_authorized()).unwrap();
        assert!(!settings.restart_pending());
    }

    // Whitespace round pasted values is the user's typing, not their intent — and
    // an untrimmed key would be stored as one the config parser barely accepts.
    #[test]
    fn pasted_values_are_trimmed() {
        let (settings, _, _dir) = settings("trim");
        let private_key = key::generate_private(Role::Agent);
        let next = Config {
            listen: "  127.0.0.1:9002\n".to_owned(),
            private_key: format!(" {private_key}\n"),
            virtual_display: true,
            virtual_display_initial_size: " 1440x900 ".to_owned(),
        };
        assert!(settings.apply(next, settings.saved_authorized()).unwrap());
        assert_eq!(settings.saved().listen, "127.0.0.1:9002");
        assert_eq!(settings.saved().private_key, private_key);
        assert_eq!(settings.saved().virtual_display_initial_size, "1440x900");
    }
}
