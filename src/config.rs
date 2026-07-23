/// Runtime configuration for the web server and the RDP target.
///
/// Kept intentionally small for the skeleton. Phase 1 will likely grow this
/// (credentials source, security options) — see docs/phase1-mvp.md.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Host/interface the web server binds to.
    pub host: String,
    /// Port the web server binds to.
    pub port: u16,
    /// RDP target host (not yet connected in the skeleton).
    pub rdp_host: String,
    /// RDP target port.
    pub rdp_port: u16,
}
