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
