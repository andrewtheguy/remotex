import AppKit
import Testing
@testable import RemotexViewer

/// The pasteboard half of the clipboard bridge — the half that stayed native.
///
/// What is asserted here is the behaviour a page cannot have: reading
/// `NSPasteboard` on a timer, writing it without a gesture, and refusing to do
/// either where it would destroy something the user has that the remote does not.
/// The panel, the reveal and the copy consent are the client's, and are tested
/// where they are.
struct ClipboardSynchronizerTests {
    @Test
    @MainActor
    func localChangeDetectionPushesOneWay() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("first", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false
        )
        var sent: [String] = []
        clipboard.sendText = { sent.append($0) }

        // Enabling pushes once: whatever is on the pasteboard when the desktop
        // comes up predates anything this could have observed, and is the value
        // somebody is most likely about to paste.
        clipboard.update(enabled: true)
        #expect(sent == ["first"])

        write("second", to: pasteboard)
        clipboard.pollPasteboard()
        #expect(sent == ["first", "second"])

        // A poll with nothing new is not a push: the change count is the whole
        // test, and re-sending an unchanged clipboard would loop against a remote
        // that echoes.
        clipboard.pollPasteboard()
        #expect(sent == ["first", "second"])
    }

    @Test
    @MainActor
    func unsolicitedRemoteContentMirrorsButPreservesANewerLocalOwner() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("local", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false
        )
        var sent: [String] = []
        clipboard.sendText = { sent.append($0) }
        clipboard.update(enabled: true)
        sent.removeAll()

        clipboard.receiveRemotePush("remote")
        #expect(pasteboard.string(forType: .string) == "remote")
        #expect(sent.isEmpty)

        // Copied here after the remote's value arrived: the newer one wins and
        // goes the other way instead of being overwritten by it.
        write("newer local", to: pasteboard)
        clipboard.receiveRemotePush("older remote")
        #expect(pasteboard.string(forType: .string) == "newer local")
        #expect(sent == ["newer local"])
    }

    /// Both echo guards, which is what keeps a value from bouncing between the two
    /// sides forever: text already mirrored is not mirrored again, and text this
    /// Mac sent is not written back when the remote echoes it.
    @Test
    @MainActor
    func repeatsAndEchoesDoNotOverwriteThePasteboard() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("local", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false
        )
        clipboard.sendText = { _ in }
        clipboard.update(enabled: true)

        clipboard.receiveRemotePush("remote")
        write("newer", to: pasteboard)
        clipboard.receiveRemotePush("remote")
        #expect(
            pasteboard.string(forType: .string) == "newer",
            "the same remote value arriving twice is not news"
        )

        // And the other guard: what this Mac pushed comes back as a server echo on
        // several VNC servers, and writing it would undo whatever was copied since.
        write("ours", to: pasteboard)
        clipboard.pollPasteboard()
        write("newer still", to: pasteboard)
        clipboard.receiveRemotePush("ours")
        #expect(pasteboard.string(forType: .string) == "newer still")
    }

    @Test
    @MainActor
    func anOversizedPasteboardIsNotSyncedAutomatically() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false
        )
        var sent: [String] = []
        clipboard.sendText = { sent.append($0) }

        write(
            String(repeating: "a", count: ClipboardSynchronizer.maximumBytes + 1),
            to: pasteboard
        )
        clipboard.update(enabled: true)
        #expect(sent.isEmpty, "one byte over the limit is still over it")

        // Not even the explicit Command-V push, which forces past the echo guard
        // but not past the ceiling.
        clipboard.pushLocalClipboard(force: true)
        #expect(sent.isEmpty)

        // At the limit it syncs, so the boundary is inclusive on both sides.
        let atLimit = String(repeating: "a", count: ClipboardSynchronizer.maximumBytes)
        write(atLimit, to: pasteboard)
        clipboard.pollPasteboard()
        #expect(sent == [atLimit])
    }

    /// Command-V forces past the echo guard, and has to: the guest is about to
    /// paste, and the last thing sent may well be exactly what it wants.
    @Test
    @MainActor
    func theCommandVPushIgnoresTheEchoGuard() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("copied", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false
        )
        var sent: [String] = []
        clipboard.sendText = { sent.append($0) }
        clipboard.update(enabled: true)
        #expect(sent == ["copied"])

        clipboard.pushLocalClipboard(force: false)
        #expect(sent == ["copied"], "nothing changed, so nothing is sent")

        clipboard.pushLocalClipboard(force: true)
        #expect(sent == ["copied", "copied"])
    }

    /// Disabled is disabled in both directions: a target without the clipboard
    /// bridge, or no desktop at all, means the pasteboard is nobody's business.
    @Test
    @MainActor
    func nothingMovesWhileDisabled() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("local", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false
        )
        var sent: [String] = []
        clipboard.sendText = { sent.append($0) }

        clipboard.pollPasteboard()
        clipboard.pushLocalClipboard(force: true)
        clipboard.receiveRemotePush("remote")
        #expect(sent.isEmpty)
        #expect(pasteboard.string(forType: .string) == "local")
    }

    /// The ceiling is the gateway's, mirrored here so an oversized value is
    /// skipped before the round trip rather than refused after it.
    @Test
    @MainActor
    func theByteLimitMatchesTheWire() {
        #expect(ClipboardSynchronizer.maximumBytes == 65_536)
        #expect(ClipboardSynchronizer.utf8ByteCount("héllo") == 6)
    }

    private func write(_ text: String, to pasteboard: NSPasteboard) {
        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
    }
}
