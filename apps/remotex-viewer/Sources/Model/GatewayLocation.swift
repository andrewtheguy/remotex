import Foundation

struct GatewayLocation: Equatable {
    let url: URL

    var origin: Origin {
        Origin(url: url)
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
