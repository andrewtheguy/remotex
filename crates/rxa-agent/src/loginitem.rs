//! Self-registration as a login item, via `SMAppService`.
//!
//! There is no install script and nothing in `~/Library/LaunchAgents`. The app
//! bundle carries its own LaunchAgent plist at
//! `Contents/Library/LaunchAgents/dev.remotex.agent.plist`, and registering it
//! hands that plist to launchd. The result:
//!
//! - drag `remotex-agent.app` to `/Applications` and open it once;
//! - it appears in **System Settings → General → Login Items**, where the user
//!   can toggle it like any other background item;
//! - uninstalling is moving the bundle to the Trash.
//!
//! The embedded plist uses launchd's `BundleProgram` key — a path *relative to
//! the bundle* — so moving the app afterwards does not break the registration.
//!
//! Two failure modes worth knowing, both reported plainly by [`register`]:
//! `SMAppService` refuses to register an improperly signed bundle, and a user who
//! has switched the item off in System Settings leaves it in
//! [`Status::RequiresApproval`] — where nothing will start it until they switch
//! it back, and no amount of re-registering helps.

use anyhow::Context;
use objc2_foundation::NSString;
use objc2_service_management::{SMAppService, SMAppServiceStatus, kSMErrorAlreadyRegistered};

/// The plist inside `Contents/Library/LaunchAgents/`. Also the launchd label.
pub const LABEL: &str = "dev.remotex.agent";

/// Registration state, as System Settings would show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Never registered, or unregistered again.
    NotRegistered,
    /// Registered and allowed to run at login.
    Enabled,
    /// Registered, but the user has it switched off in System Settings →
    /// General → Login Items. Only they can turn it back on.
    RequiresApproval,
    /// launchd has no record of the service. Observed for an agent that has
    /// simply never been registered on this machine, as well as for a bundle
    /// whose embedded plist is missing or misnamed — so it is not on its own
    /// evidence of a packaging mistake.
    NotFound,
    Unknown(isize),
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::NotRegistered => write!(f, "not registered"),
            Status::Enabled => write!(f, "enabled"),
            Status::RequiresApproval => {
                write!(f, "registered but switched off in System Settings")
            }
            Status::NotFound => write!(f, "not registered on this machine"),
            Status::Unknown(raw) => write!(f, "unknown status {raw}"),
        }
    }
}

fn service() -> objc2::rc::Retained<SMAppService> {
    let name = NSString::from_str(&format!("{LABEL}.plist"));
    // Safety: `agentServiceWithPlistName:` only reads the string, and the plist
    // it names is shipped inside our own bundle.
    unsafe { SMAppService::agentServiceWithPlistName(&name) }
}

/// Current registration state.
pub fn status() -> Status {
    // Safety: reads a property on a service object we just created.
    let raw = unsafe { service().status() };
    match raw {
        SMAppServiceStatus::NotRegistered => Status::NotRegistered,
        SMAppServiceStatus::Enabled => Status::Enabled,
        SMAppServiceStatus::RequiresApproval => Status::RequiresApproval,
        SMAppServiceStatus::NotFound => Status::NotFound,
        SMAppServiceStatus(other) => Status::Unknown(other),
    }
}

/// Register with launchd, so the agent starts now and at every login.
///
/// Idempotent from the caller's point of view: `SMAppService` reports
/// "already registered" as an error, which is not one.
pub fn register() -> anyhow::Result<()> {
    // Safety: FFI call on our own service object; errors come back as NSError.
    match unsafe { service().registerAndReturnError() } {
        Ok(()) => Ok(()),
        // "Already registered" is the desired end state, not a failure — and
        // since registration runs on every launch, treating it as an error would
        // make a correctly installed agent log a warning every login.
        Err(error) if error.code() == kSMErrorAlreadyRegistered as isize => Ok(()),
        Err(error) => Err(anyhow::anyhow!(
            "{} (code {})",
            error.localizedDescription(),
            error.code()
        )),
    }
}

/// The generation of the embedded plist that launchd is expected to be holding.
///
/// Bumped whenever `packaging/macos/embedded-launchagent.plist` changes in a way
/// launchd has to see. Generation 2 is the one with no `KeepAlive`.
pub const GENERATION: u32 = 2;

/// Where [`refresh`] records the generation launchd has been given, beside the
/// config rather than in it: it is bookkeeping about this machine's launchd, not a
/// setting anybody would edit, and Settings rewrites `config.toml` wholesale.
pub fn stamp_path() -> anyhow::Result<std::path::PathBuf> {
    let config = crate::config::default_path()?;
    let dir = config
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", config.display()))?;
    Ok(dir.join("launchagent-generation"))
}

/// Whether launchd has already been given this [`GENERATION`] of the plist.
pub fn generation_is_current(stamp: &std::path::Path) -> bool {
    std::fs::read_to_string(stamp)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        == Some(GENERATION)
}

/// Hand launchd the plist from *this* bundle, and record that it has it.
///
/// Needed because [`register`] cannot do it. `SMAppService` copies the plist to
/// launchd at registration and never looks at the bundle's again — registering an
/// already-registered service answers "already registered" and changes nothing —
/// so an upgraded bundle keeps running under the *old* job definition. Measured on
/// the test VM: a build whose plist had no `KeepAlive` went on being relaunched
/// within two seconds of every kill, because the job launchd held still had it.
///
/// Two things this deliberately does not do. It does not run on every launch —
/// macOS announces a background item being added — and it does not touch a service
/// the user has switched off in System Settings ([`Status::RequiresApproval`]):
/// taking that away and adding it back would overrule them, and somebody who
/// turned the item off is not waiting for a new plist.
///
/// **Only from a copy that is not the launchd job.** Unregistering stops the job it
/// names, so a job doing this to itself could be killed between the unregister and
/// the register — leaving the login item gone rather than refreshed. The caller
/// checks (see `hand_over_to_launchd` in `main.rs`), and a manually opened copy is
/// the natural place anyway: opening the new app is how a person installs it.
///
/// The stamp is written *first*, before anything is unregistered, so that a refresh
/// interrupted halfway cannot become a refresh that happens at every launch.
///
/// Returns whether launchd was given a new copy. Failure is the caller's to report
/// and nothing worse: the agent runs, under the previous generation's job.
pub fn refresh(stamp: &std::path::Path) -> anyhow::Result<bool> {
    if generation_is_current(stamp) {
        return Ok(false);
    }
    if let Some(dir) = stamp.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(stamp, format!("{GENERATION}\n"))
        .with_context(|| format!("writing {}", stamp.display()))?;
    // Anything else is a service launchd has no job for, which needs no refresh:
    // the `register` that runs before this one, on this launch or the next, hands
    // it this bundle's plist as a matter of course.
    if status() != Status::Enabled {
        return Ok(false);
    }
    unregister().context("unregistering to hand launchd the current plist")?;
    register().context("registering again with the current plist")?;
    Ok(true)
}

/// Unregister, so the agent no longer starts at login.
pub fn unregister() -> anyhow::Result<()> {
    // Safety: FFI call on our own service object; errors come back as NSError.
    match unsafe { service().unregisterAndReturnError() } {
        Ok(()) => Ok(()),
        Err(error) => Err(anyhow::anyhow!(
            "{} (code {})",
            error.localizedDescription(),
            error.code()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_name_matches_the_launchd_label() {
        // SMAppService looks the plist up by name inside the bundle, and launchd
        // keys the service on the Label *inside* it. Both must be
        // `dev.remotex.agent`, or registration silently finds nothing — so this
        // pins the one string that packaging/macos/embedded-launchagent.plist
        // also has to carry.
        assert_eq!(LABEL, "dev.remotex.agent");
    }

    // Running from `cargo test` there is no app bundle, so the status is
    // whatever launchd says about a service it cannot find a plist for. The
    // point is that the call is safe and returns something rather than
    // trapping in ObjC.
    #[test]
    fn status_is_readable_without_a_bundle() {
        let status = status();
        assert!(
            matches!(
                status,
                Status::NotRegistered | Status::NotFound | Status::Enabled
                    | Status::RequiresApproval | Status::Unknown(_)
            ),
            "unexpected status {status}"
        );
        // Display is used in user-facing output, so it must never be empty.
        assert!(!status.to_string().is_empty());
    }

    // The stamp is the whole of how a refresh happens once rather than at every
    // launch, so what it does with a missing, stale or malformed file matters more
    // than it looks: "current" wrongly returned for a stale generation means
    // launchd keeps an old job forever, and wrongly returned false means a
    // background-item notification every login.
    #[test]
    fn only_this_generation_counts_as_current() {
        let dir = crate::config::scratch::TempDir::new("loginitem-stamp");
        let stamp = dir.join("launchagent-generation");
        assert!(!generation_is_current(&stamp), "nothing written yet");

        std::fs::write(&stamp, format!("{GENERATION}\n")).unwrap();
        assert!(generation_is_current(&stamp));
        // Trailing whitespace is what `write` puts there; no whitespace has to
        // read the same, since nothing guarantees who wrote it last.
        std::fs::write(&stamp, GENERATION.to_string()).unwrap();
        assert!(generation_is_current(&stamp));

        std::fs::write(&stamp, (GENERATION - 1).to_string()).unwrap();
        assert!(!generation_is_current(&stamp), "an older generation");
        std::fs::write(&stamp, (GENERATION + 1).to_string()).unwrap();
        assert!(!generation_is_current(&stamp), "a newer one is not this one");
        std::fs::write(&stamp, "").unwrap();
        assert!(!generation_is_current(&stamp), "empty");
        std::fs::write(&stamp, "keepalive").unwrap();
        assert!(!generation_is_current(&stamp), "not a number");
    }

    // Same directory as the config, since that is the one place per user the agent
    // already owns.
    #[test]
    fn the_stamp_sits_beside_the_config() {
        let stamp = stamp_path().unwrap();
        let config = crate::config::default_path().unwrap();
        assert_eq!(stamp.parent(), config.parent());
        assert_eq!(
            stamp.file_name().unwrap().to_str().unwrap(),
            "launchagent-generation"
        );
    }

    #[test]
    fn every_status_has_a_human_readable_description() {
        for status in [
            Status::NotRegistered,
            Status::Enabled,
            Status::RequiresApproval,
            Status::NotFound,
            Status::Unknown(42),
        ] {
            let text = status.to_string();
            assert!(!text.is_empty(), "{status:?}");
            // The two the user has to act on should say what to do about it.
            if status == Status::RequiresApproval {
                assert!(text.contains("System Settings"), "{text}");
            }
        }
    }
}
