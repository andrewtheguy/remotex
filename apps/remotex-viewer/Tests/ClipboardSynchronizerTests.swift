import AppKit
import Testing
@testable import RemotexViewer

struct ClipboardSynchronizerTests {
    @Test
    @MainActor
    func localChangeDetectionPushesOneWayWithoutAStartupFetch() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("first", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false
        )
        var commands: [[String: Any]] = []
        clipboard.sendCommand = { commands.append($0) }

        clipboard.update(enabled: true)
        #expect(commandTypes(commands) == ["clipboard"])
        #expect(commands.first?["text"] as? String == "first")

        write("second", to: pasteboard)
        clipboard.pollPasteboard()
        #expect(commandTypes(commands) == ["clipboard", "clipboard"])
        #expect(commands.last?["text"] as? String == "second")

        clipboard.pollPasteboard()
        #expect(commands.count == 2)
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
        var commands: [[String: Any]] = []
        clipboard.sendCommand = { commands.append($0) }
        clipboard.update(enabled: true)
        commands.removeAll()

        clipboard.receiveRemotePush("remote")
        #expect(pasteboard.string(forType: .string) == "remote")
        #expect(commands.isEmpty)

        write("newer local", to: pasteboard)
        clipboard.receiveRemotePush("older remote")
        #expect(pasteboard.string(forType: .string) == "newer local")
        #expect(commands.count == 1)
        #expect(commands.first?["text"] as? String == "newer local")
    }

    @Test
    @MainActor
    func identicalRepeatsAndViewerSentEchoesDoNotOverwriteThePasteboard() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("local", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false,
            makeRequestID: { "request" }
        )
        clipboard.sendCommand = { _ in }
        clipboard.update(enabled: true)

        clipboard.receiveRemotePush("remote")
        write("newer", to: pasteboard)
        clipboard.receiveRemotePush("remote")
        #expect(pasteboard.string(forType: .string) == "newer")

        clipboard.requestFreshSnapshot()
        #expect(
            clipboard.receiveFetchResult(
                requestID: "request",
                text: "remote",
                changedAtMs: nil
            )
        )
        clipboard.reveal()
        clipboard.draft = "sent draft"
        #expect(clipboard.sendDraft())

        write("still newer", to: pasteboard)
        clipboard.receiveRemotePush("sent draft")
        #expect(pasteboard.string(forType: .string) == "still newer")
    }

    @Test
    @MainActor
    func requestedFetchNeverChangesThePasteboard() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("keep local", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false,
            makeRequestID: { "fetch-1" }
        )
        clipboard.sendCommand = { _ in }
        clipboard.update(enabled: true)
        clipboard.requestFreshSnapshot()

        #expect(
            clipboard.receiveFetchResult(
                requestID: "fetch-1",
                text: "fetched remote",
                changedAtMs: 1_725_000_123_456
            )
        )
        #expect(pasteboard.string(forType: .string) == "keep local")
        #expect(clipboard.snapshot?.text == "fetched remote")
        #expect(clipboard.snapshot?.changedAtMs == 1_725_000_123_456)

        clipboard.reveal()
        #expect(clipboard.draft == "fetched remote")
        #expect(pasteboard.string(forType: .string) == "keep local")
    }

    @Test
    @MainActor
    func explicitCopyAndSendHaveSeparateEffects() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("local", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false,
            makeRequestID: { "fetch-2" }
        )
        var commands: [[String: Any]] = []
        clipboard.sendCommand = { commands.append($0) }
        clipboard.update(enabled: true)
        commands.removeAll()
        clipboard.requestFreshSnapshot()
        commands.removeAll()
        _ = clipboard.receiveFetchResult(
            requestID: "fetch-2",
            text: "concealed",
            changedAtMs: nil
        )

        #expect(clipboard.copy())
        #expect(pasteboard.string(forType: .string) == "concealed")
        clipboard.pollPasteboard()
        #expect(commands.isEmpty, "Copy is local; Send is the remote action")

        clipboard.reveal()
        clipboard.draft = "edited ☕"
        #expect(clipboard.sendDraft())
        #expect(commandTypes(commands) == ["clipboard"])
        #expect(commands.first?["text"] as? String == "edited ☕")

        clipboard.draft = "copy draft"
        #expect(clipboard.copy())
        #expect(pasteboard.string(forType: .string) == "copy draft")

        clipboard.draft = ""
        #expect(!clipboard.copy(), "an empty copy would only clear the pasteboard")
        #expect(pasteboard.string(forType: .string) == "copy draft")
    }

    // Autosync refuses what the panel would refuse, minus the explanation: it
    // fires on a timer and on Command-V, so an oversized pasteboard must not
    // ride the socket just to be truncated at the far end.
    @Test
    @MainActor
    func anOversizedPasteboardIsNotSyncedAutomatically() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false
        )
        var commands: [[String: Any]] = []
        clipboard.sendCommand = { commands.append($0) }

        write(
            String(repeating: "a", count: ClipboardSynchronizer.maximumBytes + 1),
            to: pasteboard
        )
        clipboard.update(enabled: true)
        #expect(commands.isEmpty, "one byte over the limit is still over it")

        // Not even the explicit Command-V push, which forces past the echo
        // guard but not past the ceiling.
        clipboard.pushLocalClipboard(force: true)
        #expect(commands.isEmpty)

        // At the limit it syncs, so the boundary is inclusive on both sides.
        write(
            String(repeating: "a", count: ClipboardSynchronizer.maximumBytes),
            to: pasteboard
        )
        clipboard.pollPasteboard()
        #expect(commandTypes(commands) == ["clipboard"])
    }

    // A remote clipboard too large to transfer is reported, not mirrored: there
    // is no text to put on the pasteboard, and empty text alone would read as a
    // remote that copied nothing.
    @Test
    @MainActor
    func anOversizedRemoteClipboardIsReportedInsteadOfMirrored() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("keep local", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false,
            makeRequestID: { "fetch-oversized" }
        )
        var commands: [[String: Any]] = []
        clipboard.sendCommand = { commands.append($0) }
        clipboard.update(enabled: true)
        commands.removeAll()

        clipboard.requestFreshSnapshot()
        #expect(
            clipboard.receiveFetchResult(
                requestID: "fetch-oversized",
                text: "",
                changedAtMs: 1_725_000_123_456,
                oversizedBytes: 209_715_200
            )
        )
        #expect(clipboard.isPresented)
        #expect(clipboard.oversizedBytes == 209_715_200)
        #expect(pasteboard.string(forType: .string) == "keep local")
        #expect(clipboard.activityDescription != "UNKNOWN")

        // Copy names the reason rather than the "Nothing to copy" it would
        // otherwise share with an empty remote clipboard.
        #expect(!clipboard.copy())
        #expect(clipboard.notice == "Remote clipboard too large to transfer")
        #expect(pasteboard.string(forType: .string) == "keep local")

        // Reveal has nothing to show, so it opens the editor empty — the one way
        // on from here, and Send works from it.
        clipboard.reveal()
        #expect(clipboard.isEditing)
        #expect(clipboard.draft.isEmpty)
        #expect(clipboard.oversizedBytes == nil, "no snapshot is left to describe")
        #expect(clipboard.unavailableMessage == "Remote clipboard was too large to transfer")
        clipboard.draft = "typed by hand"
        #expect(clipboard.sendDraft())
        #expect(commands.last?["text"] as? String == "typed by hand")
    }

    // An unsolicited oversized copy is panel state only: it must never reach the
    // pasteboard, and must not be mistaken for the previous value either.
    @Test
    @MainActor
    func anOversizedRemotePushUpdatesAnOpenPanelButNotThePasteboard() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        write("keep local", to: pasteboard)

        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false,
            makeRequestID: { "fetch-then-push" }
        )
        clipboard.sendCommand = { _ in }
        clipboard.update(enabled: true)
        clipboard.requestFreshSnapshot()
        _ = clipboard.receiveFetchResult(
            requestID: "fetch-then-push",
            text: "small remote value",
            changedAtMs: nil
        )

        clipboard.noteRemoteOversized(bytes: 209_715_200)
        #expect(clipboard.oversizedBytes == 209_715_200)
        #expect(clipboard.snapshot?.text.isEmpty == true)
        #expect(
            pasteboard.string(forType: .string) == "keep local",
            "an oversized push has nothing to mirror"
        )

        // The same value arriving again once it fits is not an echo of what the
        // panel just showed, so it still mirrors.
        clipboard.receiveRemotePush("small remote value")
        #expect(pasteboard.string(forType: .string) == "small remote value")
    }

    // The web side answers every request, so a failed read reaches the panel
    // without waiting out the local deadline.
    @Test
    @MainActor
    func aFailedFetchOpensTheUnavailableEditorImmediately() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false,
            makeRequestID: { "fetch-3" }
        )
        clipboard.sendCommand = { _ in }
        clipboard.update(enabled: true)
        clipboard.requestFreshSnapshot()

        clipboard.fetchUnavailable(requestID: "stale")
        #expect(clipboard.isFetching, "a stale answer cannot end the fetch")
        #expect(!clipboard.isPresented)

        clipboard.fetchUnavailable(requestID: "fetch-3")
        #expect(!clipboard.isFetching)
        #expect(clipboard.isPresented)
        #expect(clipboard.isEditing)
        #expect(clipboard.snapshot == nil)
        #expect(clipboard.unavailableMessage == "Remote clipboard unavailable")
    }

    @Test
    @MainActor
    func requestMatchingTimeoutAndCloseResetThePanel() {
        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        var requestIDs = ["request-a", "request-b"]
        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false,
            makeRequestID: { requestIDs.removeFirst() }
        )
        clipboard.sendCommand = { _ in }
        clipboard.update(enabled: true)

        clipboard.requestFreshSnapshot()
        #expect(clipboard.isFetching)
        #expect(clipboard.pendingRequestID == "request-a")
        #expect(
            !clipboard.receiveFetchResult(
                requestID: "stale",
                text: "stale text",
                changedAtMs: nil
            )
        )
        #expect(clipboard.isFetching)
        #expect(!clipboard.isPresented)

        #expect(
            clipboard.receiveFetchResult(
                requestID: "request-a",
                text: "current",
                changedAtMs: 1_725_000_123_456
            )
        )
        #expect(clipboard.isPresented)
        #expect(!clipboard.isEditing)
        #expect(clipboard.activityDescription != "UNKNOWN")

        clipboard.closePanel()
        #expect(!clipboard.isPresented)
        #expect(clipboard.snapshot == nil)
        #expect(clipboard.draft.isEmpty)
        #expect(clipboard.notice == nil)

        clipboard.requestFreshSnapshot()
        clipboard.fetchUnavailable(requestID: "request-b")
        #expect(clipboard.isPresented)
        #expect(clipboard.isEditing)
        #expect(clipboard.draft.isEmpty)
        #expect(clipboard.unavailableMessage == "Remote clipboard unavailable")
        #expect(!clipboard.sendDraft(), "an empty draft would wipe the remote")
        clipboard.draft = "typed into the unavailable editor"
        #expect(clipboard.sendDraft(), "the timeout editor keeps Send usable")

        clipboard.closePanel()
        #expect(clipboard.unavailableMessage == nil)
        #expect(!clipboard.isEditing)
    }

    @Test
    @MainActor
    func crc32AndUtf8LimitsMatchTheWebPanel() {
        #expect(ClipboardSynchronizer.crc32Hex("123456789") == "cbf43926")
        #expect(ClipboardSynchronizer.crc32Hex("") == "00000000")
        #expect(ClipboardSynchronizer.utf8ByteCount("é☕") == 5)

        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        let clipboard = ClipboardSynchronizer(
            pasteboard: pasteboard,
            startsPolling: false,
            makeRequestID: { "limit" }
        )
        clipboard.sendCommand = { _ in }
        clipboard.update(enabled: true)
        clipboard.requestFreshSnapshot()
        clipboard.fetchUnavailable(requestID: "limit")

        clipboard.draft = String(repeating: "\u{00E9}", count: 32_768)
        #expect(clipboard.draftByteCount == ClipboardSynchronizer.maximumBytes)
        #expect(!clipboard.isOverByteLimit)
        #expect(clipboard.sendDraft())

        clipboard.draft.append("a")
        #expect(clipboard.draftByteCount == ClipboardSynchronizer.maximumBytes + 1)
        #expect(clipboard.isOverByteLimit)
        #expect(!clipboard.sendDraft())
    }

    @MainActor
    private func write(_ text: String, to pasteboard: NSPasteboard) {
        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
    }

    private func commandTypes(_ commands: [[String: Any]]) -> [String] {
        commands.compactMap { $0["type"] as? String }
    }
}
