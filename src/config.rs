use std::path::PathBuf;

/// RDP security negotiation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Security {
    /// Advertise both TLS and NLA/CredSSP; the server picks the strongest.
    Auto,
    /// Require NLA/CredSSP (network-level auth before the session).
    Nla,
    /// Plain TLS security only — no NLA; the remote shows a graphical login.
    Tls,
}

impl Security {
    /// `(enable_tls, enable_credssp)` for the IronRDP connector config.
    pub fn flags(self) -> (bool, bool) {
        match self {
            Security::Auto => (true, true),
            Security::Nla => (false, true),
            Security::Tls => (true, false),
        }
    }
}

/// Runtime configuration for the web server and the RDP target.
///
/// Credentials live here (server-side) and are used during the RDP handshake.
/// They are never sent to the browser — see docs/phase1-mvp.md.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Host/interface the web server binds to.
    pub host: String,
    /// Port the web server binds to.
    pub port: u16,
    /// RDP target host.
    pub rdp_host: String,
    /// RDP target port.
    pub rdp_port: u16,
    /// RDP username.
    pub rdp_username: String,
    /// RDP password (never leaves the server).
    pub rdp_password: String,
    /// Optional RDP domain.
    pub rdp_domain: Option<String>,
    /// Initial desktop width requested from the server.
    pub rdp_width: u16,
    /// Initial desktop height requested from the server.
    pub rdp_height: u16,
    /// RDP security negotiation mode.
    pub rdp_security: Security,
    /// Directory holding the built frontend (index.html + assets), served from
    /// disk. Defaults to [`default_static_dir`].
    pub static_dir: PathBuf,
}

/// The install root, derived from the running binary's own location.
///
/// The binary is shipped at `<root>/bin/rdpweb`, so its assets and config live
/// at `<root>/share/…` and `<root>/etc/…`. We canonicalize `current_exe` so a
/// launcher symlink (e.g. `/usr/local/bin/rdpweb` → `…/current/bin/rdpweb`)
/// resolves to the real versioned directory. Returns `None` in odd environments
/// where the executable path can't be determined.
pub fn install_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    // <root>/bin/rdpweb → <root>
    Some(exe.parent()?.parent()?.to_path_buf())
}

/// Default location of the built frontend.
///
/// Prefers the installed layout (`<root>/share/rdpweb/web`); falls back to
/// `frontend/dist` relative to the working directory for `cargo run` in a
/// checkout. Override with `--static-dir` / `RDPWEB_STATIC_DIR`.
pub fn default_static_dir() -> PathBuf {
    if let Some(root) = install_root() {
        let installed = root.join("share/rdpweb/web");
        if installed.is_dir() {
            return installed;
        }
    }
    PathBuf::from("frontend/dist")
}

/// Path of the installed env/config file to load at startup, if any.
///
/// `RDPWEB_ENV_FILE` overrides; otherwise `<root>/etc/rdpweb.env` when present.
/// Values here seed the `RDPWEB_*` environment; real environment variables and
/// CLI flags still take precedence (see `main.rs`).
pub fn default_env_file() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RDPWEB_ENV_FILE") {
        return Some(PathBuf::from(path));
    }
    let candidate = install_root()?.join("etc/rdpweb.env");
    candidate.is_file().then_some(candidate)
}
