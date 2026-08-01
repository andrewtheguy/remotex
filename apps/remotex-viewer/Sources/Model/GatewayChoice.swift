import Foundation

/// Which gateway this launch talks to.
///
/// Selects the bundled or remote gateway. Both expose `/api/config`,
/// `/api/targets`, `/api/session`, and `/ws` with the same shapes;
/// `GatewayCredential` handles their different authentication headers. This choice
/// controls only process/config ownership and whether a 401 restarts the embedded
/// gateway or returns a remote connection to login.
enum GatewayChoice: Equatable, Sendable {
    /// The bundled gateway on an ephemeral loopback port; see `EmbeddedGateway`.
    case embedded
    /// A login-protected gateway at a user-supplied address.
    case remote(GatewayLocation)

    var isEmbedded: Bool {
        self == .embedded
    }

    /// User-visible address; nil for the ephemeral embedded gateway.
    var location: GatewayLocation? {
        switch self {
        case .embedded:
            nil
        case .remote(let location):
            location
        }
    }

    /// What the login screen and the picker put under the branding.
    var label: String {
        switch self {
        case .embedded:
            "This Mac"
        case .remote(let location):
            location.url.absoluteString
        }
    }
}
