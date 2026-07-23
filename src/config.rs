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
}
