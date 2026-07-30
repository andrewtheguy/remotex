import Foundation

/// The handful of things this client remembers between launches, in the instance
/// directory with everything else.
///
/// A JSON file rather than `UserDefaults`, because the instance directory is now the
/// unit of isolation and a defaults suite is not part of it: `--instance-dir /tmp/qa`
/// has to mean *everything* about that run is over there, and a suite lives in the
/// user's own `Preferences` whatever the rest of the app was told. That is the same
/// trap `--settings` existed to work around, one layer down.
///
/// Failures are ignored deliberately. A preference that cannot be read is a default,
/// and a preference that cannot be written is one that will not be remembered —
/// neither is worth an alert in front of somebody who pressed a menu item.
@MainActor
final class ViewerPreferences {
    /// Whether a Command chord is translated to the remote's Control. Default on;
    /// see `KeyboardTranslator`.
    var macOSKeyboardOverridesEnabled: Bool {
        didSet {
            guard macOSKeyboardOverridesEnabled != oldValue else {
                return
            }
            save()
        }
    }

    private let url: URL?

    /// Stored preferences, defaulted for anything absent.
    private struct Stored: Codable {
        var macOSKeyboardOverridesEnabled: Bool?
    }

    /// The preferences at `url`. A `nil` url is an in-memory set, which is what a
    /// test wants and what an unwritable instance directory falls back to.
    init(url: URL?) {
        self.url = url
        let stored = url
            .flatMap { try? Data(contentsOf: $0) }
            .flatMap { try? JSONDecoder().decode(Stored.self, from: $0) }
        macOSKeyboardOverridesEnabled = stored?.macOSKeyboardOverridesEnabled ?? true
    }

    private func save() {
        guard let url else {
            return
        }
        let stored = Stored(macOSKeyboardOverridesEnabled: macOSKeyboardOverridesEnabled)
        guard let data = try? JSONEncoder().encode(stored) else {
            return
        }
        try? data.write(to: url, options: .atomic)
    }
}
