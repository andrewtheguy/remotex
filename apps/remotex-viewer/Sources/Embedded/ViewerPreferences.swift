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
/// It holds a live credential (`remoteSessionToken`), so the file is written
/// owner-only inside a directory that already is — see `InstanceDirectory.create`.
/// The config beside it holds every target's password, so this is the posture that
/// was already being kept rather than a new one being taken.
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

    /// Whether a new connection should follow this window's size where the target
    /// allows it. Default off. Set from the picker's "… if compatible" toggle and
    /// from the View menu's live "Auto Resize" alike — one remembered value, two
    /// places to set it — and applied to a session only where `connected` says the
    /// target can honour it.
    var autoResizeByDefault: Bool {
        didSet {
            guard autoResizeByDefault != oldValue else {
                return
            }
            save()
        }
    }

    /// Whether a new connection should play the remote's sound where the target
    /// carries it. Default off. The same one-value-two-places arrangement as
    /// `autoResizeByDefault`: the picker's toggle and the Remote menu's live
    /// "Enable Audio" write it, and it is applied on connect only when the target
    /// actually offers audio.
    var audioByDefault: Bool {
        didSet {
            guard audioByDefault != oldValue else {
                return
            }
            save()
        }
    }

    /// Which option the home screen offers first: the last one that worked.
    ///
    /// The choice is remembered and the screen is still shown. Remembering it and
    /// *skipping* the screen is the arrangement this replaced — a single option
    /// chosen once and never asked about again is exactly what left no way to reach
    /// the other one.
    var prefersRemoteGateway: Bool {
        didSet {
            guard prefersRemoteGateway != oldValue else {
                return
            }
            save()
        }
    }

    /// The last remote address that answered, so the field comes up filled in.
    /// Written only after the gateway has answered, so a typo never becomes the
    /// address the next launch starts from.
    var remoteGatewayAddress: String? {
        didSet {
            guard remoteGatewayAddress != oldValue else {
                return
            }
            save()
        }
    }

    /// The remote gateway's session cookie, so quitting the app does not mean typing
    /// the password again.
    ///
    /// Kept because the browser keeps one and the app used to: its cookie jar was
    /// `HTTPCookieStorage.shared`, which is why quitting still landed you on the
    /// picker. Held here instead because that jar matches by host and ignores the
    /// port, so two instances pointed at two gateways on one host shared a login and
    /// each new one logged the other out.
    ///
    /// Sessions are in-memory on the gateway with a sliding six-hour expiry, so a
    /// stored token is a hint and never an assumption: `/api/auth/status` is what
    /// decides, and a token it does not recognise costs one request and the login
    /// screen.
    var remoteSessionToken: String? {
        didSet {
            guard remoteSessionToken != oldValue else {
                return
            }
            save()
        }
    }

    private let url: URL?

    /// Stored preferences, defaulted for anything absent.
    private struct Stored: Codable {
        var macOSKeyboardOverridesEnabled: Bool?
        var autoResizeByDefault: Bool?
        var audioByDefault: Bool?
        var prefersRemoteGateway: Bool?
        var remoteGatewayAddress: String?
        var remoteSessionToken: String?
    }

    /// The preferences at `url`. A `nil` url is an in-memory set, which is what a
    /// test wants and what an unwritable instance directory falls back to.
    init(url: URL?) {
        self.url = url
        let stored = url
            .flatMap { try? Data(contentsOf: $0) }
            .flatMap { try? JSONDecoder().decode(Stored.self, from: $0) }
        macOSKeyboardOverridesEnabled = stored?.macOSKeyboardOverridesEnabled ?? true
        autoResizeByDefault = stored?.autoResizeByDefault ?? false
        audioByDefault = stored?.audioByDefault ?? false
        prefersRemoteGateway = stored?.prefersRemoteGateway ?? false
        remoteGatewayAddress = stored?.remoteGatewayAddress
        remoteSessionToken = stored?.remoteSessionToken
    }

    private func save() {
        guard let url else {
            return
        }
        let stored = Stored(
            macOSKeyboardOverridesEnabled: macOSKeyboardOverridesEnabled,
            autoResizeByDefault: autoResizeByDefault,
            audioByDefault: audioByDefault,
            prefersRemoteGateway: prefersRemoteGateway,
            remoteGatewayAddress: remoteGatewayAddress,
            remoteSessionToken: remoteSessionToken
        )
        guard let data = try? JSONEncoder().encode(stored) else {
            return
        }
        try? data.write(to: url, options: .atomic)
        // After the write, because `.atomic` replaces the file: an `0600` set on the
        // previous inode says nothing about the one that replaced it.
        try? FileManager.default.setAttributes(
            [.posixPermissions: 0o600],
            ofItemAtPath: url.path
        )
    }
}
