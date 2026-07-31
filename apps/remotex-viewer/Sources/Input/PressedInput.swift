import Foundation

/// What is currently held down on the remote, so it can all be let go.
///
/// The protocol has no release-everything message: the client tracks what it
/// pressed and sends one release per code, the way
/// `frontend/src/useRemoteDesktop.ts` does in `releaseAll`. (The old host bridge
/// did have a `releaseKeys` command, which is why this is easy to port wrong —
/// there is nothing on the wire it maps to.)
///
/// Worth its own type because a modifier left down on the remote is this
/// client's worst failure mode, and the paths that must release are many: the
/// window losing key status, the app deactivating, focus leaving the surface, a
/// target switch, a socket close, a takeover, teardown.
struct PressedInput: Equatable {
    private var keys: Set<String> = []
    private var buttons: Set<MouseButton> = []

    var isEmpty: Bool {
        keys.isEmpty && buttons.isEmpty
    }

    mutating func record(code: String, pressed: Bool) {
        if pressed {
            keys.insert(code)
        } else {
            keys.remove(code)
        }
    }

    /// Whether this button is recorded as held. Asked before forwarding a release
    /// that arrives with input otherwise closed — see `AppModel.sendMouseButton`,
    /// which is the one caller that may send past its own gate.
    func isHeld(_ button: MouseButton) -> Bool {
        buttons.contains(button)
    }

    mutating func record(button: MouseButton, pressed: Bool) {
        if pressed {
            buttons.insert(button)
        } else {
            buttons.remove(button)
        }
    }

    /// Everything needed to let go of what is held, clearing the record so a
    /// second call — two focus notifications for one event, say — sends nothing.
    ///
    /// `caps: false` on release: the gateway releases the keysym it recorded at
    /// press time, so the lock state is not consulted.
    ///
    /// Sorted for a stable order. Nothing on the wire depends on it; a test
    /// asserting on the output does.
    mutating func takeReleaseMessages() -> [ClientMessage] {
        let messages =
            keys.sorted().map { ClientMessage.key(code: $0, pressed: false, caps: false) }
                + buttons.sorted { $0.rawValue < $1.rawValue }
                .map { ClientMessage.mouseButton(button: $0, pressed: false, clicks: 1) }
        keys.removeAll()
        buttons.removeAll()
        return messages
    }
}
