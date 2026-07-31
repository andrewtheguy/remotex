import Foundation

/// Which gateway this launch talks to.
///
/// The app carries a gateway *and* can be pointed at one, and the difference between
/// them is smaller than it looks: `/api/config`, `/api/targets`, `/api/session` and
/// `/ws` are the same routes with the same shapes either way. What actually differs
/// is one thing — how a request proves it may be made — and that is
/// [`GatewayCredential`], not this.
///
/// So this exists to answer the questions that are genuinely about *which* gateway:
/// whether there is a process of ours to start and stop, whether the configuration
/// panel edits anything the session can see, and whether a 401 means "restart the
/// gateway" or "sign in again".
enum GatewayChoice: Equatable, Sendable {
    /// The gateway in this bundle, on a loopback port it picks. No address and no
    /// credentials — see `EmbeddedGateway`.
    case embedded
    /// A gateway somewhere else, reached over HTTP with a login. The one thing the
    /// embedded gateway cannot be: on the other side of a slow link, where it is the
    /// gateway rather than this Mac that should be doing the talking to the target.
    case remote(GatewayLocation)

    var isEmbedded: Bool {
        self == .embedded
    }

    /// The address, for the screens that show it. Nil for the embedded gateway,
    /// whose port is not known until it has bound one and is not worth showing when
    /// it is.
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
