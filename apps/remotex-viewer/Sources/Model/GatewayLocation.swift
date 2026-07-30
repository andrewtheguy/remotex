import Foundation

struct GatewayLocation: Equatable {
    let url: URL

    var origin: Origin {
        Origin(url: url)
    }

    /// This app's own gateway, at the port it reported.
    ///
    /// Built rather than parsed: the port comes from a `UInt16` the gateway printed
    /// after binding it, so there is no text to be wrong and no failure to handle.
    /// `127.0.0.1` and not `localhost` — the gateway binds one address and that is
    /// the one it binds, so resolving a name here could only find the other loopback
    /// and fail.
    static func loopback(port: UInt16) -> GatewayLocation {
        var components = URLComponents()
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = Int(port)
        components.path = "/"
        // Unwrapped deliberately: these four fields always compose, and a `nil` here
        // would mean Foundation cannot build `http://127.0.0.1:1/`.
        return GatewayLocation(url: components.url!)
    }

    static func parse(_ input: String) throws -> GatewayLocation {
        let trimmed = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            throw GatewayLocationError.empty
        }
        let candidate = trimmed.contains("://") ? trimmed : "https://\(trimmed)"
        guard var components = URLComponents(string: candidate),
              let scheme = components.scheme?.lowercased(),
              scheme == "http" || scheme == "https",
              components.host?.isEmpty == false
        else {
            throw GatewayLocationError.invalid
        }
        guard components.user == nil, components.password == nil,
              components.query == nil, components.fragment == nil
        else {
            throw GatewayLocationError.invalid
        }
        components.scheme = scheme
        // A host is case-insensitive, so `Example.test` and `example.test` are one
        // gateway. Normalized here because two locations that differ only in case
        // would otherwise compare unequal, and `AppModel.connectToGateway` reads
        // that as a move to another host: it tears the session down and drops the
        // login cookie. Left alone when it is already lowercase, so nothing
        // round-trips an IPv6 literal through the setter for no reason.
        if let host = components.host, host != host.lowercased() {
            components.host = host.lowercased()
        }
        components.path = "/"
        guard let url = components.url else {
            throw GatewayLocationError.invalid
        }
        return GatewayLocation(url: url)
    }

    struct Origin: Equatable {
        let scheme: String
        let host: String
        let port: Int

        init(url: URL) {
            scheme = url.scheme?.lowercased() ?? ""
            host = url.host?.lowercased() ?? ""
            port = url.port ?? (scheme == "https" ? 443 : 80)
        }

        func contains(_ url: URL) -> Bool {
            Origin(url: url) == self
        }
    }
}

enum GatewayLocationError: LocalizedError {
    case empty
    case invalid

    var errorDescription: String? {
        switch self {
        case .empty:
            "Enter the remotex gateway address."
        case .invalid:
            "Use an HTTP or HTTPS gateway address without credentials, a query, or a fragment."
        }
    }
}
