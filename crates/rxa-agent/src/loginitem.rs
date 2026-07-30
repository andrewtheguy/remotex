//! Start at login: a plain LaunchAgent in `~/Library/LaunchAgents`, naming this
//! executable by **absolute path**.
//!
//! ```text
//! ~/Library/LaunchAgents/dev.remotex.agent.plist
//!   ProgramArguments[0] = /Applications/remotex-agent.app/Contents/MacOS/remotex-agent
//! ```
//!
//! Two properties, and the whole module exists for them:
//!
//! **The path is absolute**, so `launchctl kickstart` can only ever start that one
//! binary. **Nothing writes the plist implicitly** — only [`install`], from the
//! menu's Start at Login or `--install-launchagent`. A copy of the app that merely
//! runs changes nothing at all.
//!
//! This replaced `SMAppService`, which registered a plist *embedded in the bundle*
//! whose `BundleProgram` was a path relative to whichever copy registered it. Since
//! every copy registered itself on launch, the job's identity belonged to whatever
//! ran most recently — so opening the app once from a mounted release DMG (which
//! the packaging check in `CLAUDE.md` asks for) captured the job, and every
//! `kickstart` afterwards silently started that old binary. Ejecting the image left
//! launchd unable to spawn it at all: 15 failures, `EX_CONFIG`, penalty box, while
//! the installed bundle reported the right version to anyone who asked it.
//!
//! Worse, it could not repair itself. Registering an already-registered service
//! answered "already registered" and left the path alone, and the unregister +
//! register that *would* have repointed it was gated behind a stamp file recording
//! which plist generation launchd had — which said "current", because the
//! generation had not changed. The stamp, the generation counter and the embedded
//! plist are all gone with it: an absolute path needs no cache-busting, because
//! there is nothing to resolve.
//!
//! See `docs/roadmap.md`. The login-window service planned there needs an
//! absolute-path plist regardless.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// The launchd label, and the plist's file name.
pub const LABEL: &str = "dev.remotex.agent";

/// Where the LaunchAgent plist belongs for this user.
pub fn plist_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// This executable's absolute path — what a login item would name.
///
/// Canonicalized, so a symlinked or `.`-relative launch still writes a path
/// launchd can resolve years later.
pub fn program() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot read this executable's path")?;
    exe.canonicalize()
        .with_context(|| format!("cannot resolve {}", exe.display()))
}

/// Whether `exe` is somewhere a login item may point at.
///
/// The rule that makes the old failure impossible: a disk image is mounted under
/// `/Volumes` and will not be there at the next login, so a plist naming one is a
/// job that cannot start. The Trash is the same story with a different ending.
///
/// Deliberately *not* "must be `/Applications`" — a build run from a checkout is a
/// legitimate thing to start at login on a test VM, and refusing it would only
/// teach people to work around this.
pub fn is_installable(exe: &Path) -> bool {
    !exe.starts_with("/Volumes") && !exe.components().any(|c| c.as_os_str() == ".Trash")
}

/// What the login item currently says, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No plist. The agent runs when somebody opens it, and not at login.
    NotInstalled,
    /// Installed, naming this executable.
    Installed,
    /// Installed, naming a *different* executable — an older bundle, or one that
    /// is no longer there.
    ///
    /// Its own state because it used to be invisible and cost hours: this is what
    /// makes `kickstart` start something other than what you just deployed, and now
    /// the menu and the log both say so.
    Elsewhere(PathBuf),
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::NotInstalled => write!(f, "not installed"),
            Status::Installed => write!(f, "starts at login"),
            Status::Elsewhere(program) => write!(
                f,
                "starts a different copy at login: {}",
                program.display()
            ),
        }
    }
}

/// Read the login item's state.
pub fn status() -> Status {
    let Ok(path) = plist_path() else {
        return Status::NotInstalled;
    };
    let Some(installed) = installed_program(&path) else {
        return Status::NotInstalled;
    };
    match program() {
        Ok(mine) if mine == installed => Status::Installed,
        // Unresolvable own path is not the interesting case, and reporting the
        // installed one is still the more useful answer.
        _ => Status::Elsewhere(installed),
    }
}

/// The program a plist names, via `plutil` so a hand-edited or binary plist reads
/// the same as one this module wrote.
fn installed_program(plist: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "ProgramArguments.0", "raw", "-o", "-"])
        .arg(plist)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// Write the login item for this executable and load it.
///
/// Always rewrites and re-bootstraps rather than checking first: launchd holds the
/// job it was given at bootstrap, so a plist edited underneath it changes nothing
/// until it is reloaded. Doing both unconditionally is what replaces the generation
/// stamp the old implementation needed — and it is idempotent, so it also repairs a
/// [`Status::Elsewhere`].
pub fn install() -> anyhow::Result<()> {
    let exe = program()?;
    anyhow::ensure!(
        is_installable(&exe),
        "refusing to start {} at login: it is on a disk image or in the Trash, so the job \
         could not run at the next login. Copy remotex-agent.app to /Applications and try \
         from there.",
        exe.display()
    );

    let path = plist_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Temp file and rename, so an interrupted write cannot leave launchd a
    // truncated plist to choke on at the next login.
    let temp = path.with_extension("plist.new");
    std::fs::write(&temp, plist_xml(&exe)).with_context(|| format!("writing {}", temp.display()))?;
    if let Err(e) = std::fs::rename(&temp, &path) {
        // Best effort, and worth doing: this directory is one launchd reads, so a
        // half-written file left in it is litter in the worst possible place.
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("replacing {}", path.display()));
    }

    // Not an error: it is only loaded if this has been done before.
    let _ = launchctl(&["bootout", &service()]);
    // **Before the bootstrap, not after.** `enable`/`disable` act on the service
    // *name* and need no loaded job, and launchd refuses to bootstrap a service
    // that has been disabled — `Bootstrap failed: 5: Input/output error`. Enabling
    // afterwards was unreachable in exactly the case it existed for: the bootstrap
    // returned first, so a login item switched off once in System Settings could
    // never be switched back on from here, and every retry failed identically.
    //
    // Best-effort, like the bootout: a service that was never disabled has nothing
    // to enable, and if it *was* and this somehow fails, the bootstrap below says so.
    let _ = launchctl(&["enable", &service()]);
    launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])
        .context("loading the login item")?;
    Ok(())
}

/// Remove the login item. The agent then runs only when somebody opens it.
///
/// Note this stops the job as well, which is the running agent when the job *is*
/// the running agent — the same thing unchecking the box always did.
pub fn uninstall() -> anyhow::Result<()> {
    let path = plist_path()?;
    let _ = launchctl(&["bootout", &service()]);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// `gui/<uid>`, the domain a per-user LaunchAgent lives in.
pub fn domain() -> String {
    format!("gui/{}", crate::uid())
}

/// `gui/<uid>/dev.remotex.agent`, the service as `launchctl` names it.
pub fn service() -> String {
    format!("{}/{LABEL}", domain())
}

fn launchctl(args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new("/bin/launchctl")
        .args(args)
        .output()
        .with_context(|| format!("cannot run launchctl {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "launchctl {} failed ({}){}",
        args.join(" "),
        output.status,
        match String::from_utf8_lossy(&output.stderr).trim() {
            "" => String::new(),
            stderr => format!(": {stderr}"),
        }
    );
    Ok(())
}

/// The plist, generated rather than shipped: it carries an absolute path, so it
/// cannot be a static file in the bundle.
///
/// `RunAtLoad` is what "start at login" means. `ProcessType Interactive` keeps the
/// agent off the throttled background tier, so macOS does not slow its capture and
/// encode work down.
///
/// No `KeepAlive`, deliberately, and this is the second time that decision has been
/// worth writing down: it used to carry `KeepAlive { SuccessfulExit: false }` for
/// crash recovery, and the cost was stale instances — any death by signal counts as
/// an unsuccessful exit, so launchd relaunched within ~2s from whatever was on disk
/// at that moment, which mid-upgrade is the *old* binary holding the port the new
/// one wants. An agent that has died now stays dead, which is visible (no menu bar,
/// connections refused) rather than quietly wrong.
///
/// No `StandardOutPath`/`StandardErrorPath`: launchd does not expand `~`, and the
/// agent redirects its own logging to `~/Library/Logs/remotex-agent.log` when it is
/// not on a terminal.
///
/// This is a LaunchAgent and not a LaunchDaemon because both TCC grants need a GUI
/// session with a window server. Login-window support would need a privileged,
/// system-wide agent targeting `LoginWindow` as well as `Aqua`; see
/// `docs/roadmap.md`.
pub fn plist_xml(program: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- Written by remotex-agent (see crates/rxa-agent/src/loginitem.rs). The program
     path is absolute on purpose: it is what stops launchd starting some other copy
     of the app. Rewritten wholesale by Start at Login, so edits here are lost. -->
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>RUST_LOG</key>
		<string>info</string>
	</dict>
</dict>
</plist>
"#,
        escape(&program.to_string_lossy())
    )
}

/// XML-escape a path. Rare but real: `&` is legal in a directory name.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_name_matches_the_launchd_label() {
        // launchd keys the service on the Label inside the plist, and `launchctl
        // bootout`/`kickstart` name it by label too, so the file name and the
        // Label have to agree.
        assert_eq!(LABEL, "dev.remotex.agent");
        let path = plist_path().unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "dev.remotex.agent.plist"
        );
        assert!(path.ends_with("Library/LaunchAgents/dev.remotex.agent.plist"));
        assert!(path.is_absolute(), "launchd cannot resolve a relative path");
    }

    // The property the whole module exists for.
    #[test]
    fn the_plist_names_the_program_by_absolute_path() {
        let xml = plist_xml(Path::new("/Applications/remotex-agent.app/Contents/MacOS/remotex-agent"));
        assert!(
            xml.contains("<string>/Applications/remotex-agent.app/Contents/MacOS/remotex-agent</string>"),
            "{xml}"
        );
        assert!(xml.contains("<key>ProgramArguments</key>"), "{xml}");
        // The key that made a stray copy able to capture the job. It must not
        // come back: `BundleProgram` resolves against whoever registered it.
        assert!(!xml.contains("BundleProgram"), "{xml}");
        // And the one that used to resurrect old binaries mid-upgrade.
        assert!(!xml.contains("KeepAlive"), "{xml}");
        assert!(xml.contains("<key>RunAtLoad</key>"), "{xml}");
    }

    #[test]
    fn a_path_with_xml_significant_characters_survives() {
        let xml = plist_xml(Path::new("/Users/a&b/<app>/remotex-agent"));
        assert!(xml.contains("/Users/a&amp;b/&lt;app&gt;/remotex-agent"), "{xml}");
        // No raw `&` outside the entities we wrote, or the plist will not parse.
        assert!(!xml.contains("a&b"), "{xml}");
    }

    // A disk image is the case that cost the afternoon: mounted now, gone at the
    // next login, and a plist naming one is a job launchd puts in the penalty box.
    #[test]
    fn a_login_item_is_refused_for_a_path_that_will_not_be_there() {
        assert!(!is_installable(Path::new(
            "/Volumes/remotex-agent 0.0.59/remotex-agent.app/Contents/MacOS/remotex-agent"
        )));
        assert!(!is_installable(Path::new(
            "/Users/andrew/.Trash/remotex-agent.app/Contents/MacOS/remotex-agent"
        )));

        // And the two that are fine: the installed copy, and a build from a
        // checkout, which is a real thing to run on a test VM.
        assert!(is_installable(Path::new(
            "/Applications/remotex-agent.app/Contents/MacOS/remotex-agent"
        )));
        assert!(is_installable(Path::new(
            "/Users/andrew/remotex/dist/remotex-agent.app/Contents/MacOS/remotex-agent"
        )));
    }

    #[test]
    fn every_status_says_something_a_person_can_act_on() {
        assert_eq!(Status::NotInstalled.to_string(), "not installed");
        assert_eq!(Status::Installed.to_string(), "starts at login");
        // The mismatch has to name the other copy — that *is* the diagnosis.
        let elsewhere = Status::Elsewhere(PathBuf::from("/Volumes/old/remotex-agent"));
        assert!(elsewhere.to_string().contains("/Volumes/old/remotex-agent"));
        assert!(elsewhere.to_string().contains("different"));
    }

    // Round trip through the real `plutil`, which is what `status` reads with: a
    // plist this module writes must be one macOS can parse.
    #[test]
    fn plutil_reads_back_the_program_this_module_wrote() {
        let dir = crate::config::scratch::TempDir::new("loginitem-plist");
        let plist = dir.join("dev.remotex.agent.plist");
        let exe = Path::new("/Applications/remotex-agent.app/Contents/MacOS/remotex-agent");
        std::fs::write(&plist, plist_xml(exe)).unwrap();

        assert_eq!(installed_program(&plist).as_deref(), Some(exe));
        // And a file that is not a plist at all reads as "nothing installed"
        // rather than panicking or inventing a path.
        let junk = dir.join("junk.plist");
        std::fs::write(&junk, "not a plist").unwrap();
        assert_eq!(installed_program(&junk), None);
        assert_eq!(installed_program(&dir.join("absent.plist")), None);
    }

    #[test]
    fn the_service_name_is_the_one_launchctl_wants() {
        assert_eq!(domain(), format!("gui/{}", crate::uid()));
        assert_eq!(service(), format!("gui/{}/dev.remotex.agent", crate::uid()));
    }
}
