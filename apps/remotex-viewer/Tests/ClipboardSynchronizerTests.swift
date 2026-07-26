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
