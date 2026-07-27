import AppKit
import Foundation
import Observation

struct NativeClipboardSnapshot: Equatable {
    let text: String
    let changedAtMs: Int64?
    /// Set when the remote's clipboard was refused for exceeding
    /// ``ClipboardSynchronizer/maximumBytes``, to the size it actually is.
    /// `text` is empty then — which on its own would read as "the remote has
    /// copied nothing", the very thing this tells apart.
    let oversizedBytes: Int64?
}

@MainActor
@Observable
final class ClipboardSynchronizer {
    static let maximumBytes = 65_536

    /// Where a clipboard message goes. Typed rather than a dictionary, so this
    /// speaks the same `ClientMessage` the socket does and cannot invent a shape
    /// the gateway would drop.
    var send: ((ClientMessage) -> Void)?

    private(set) var isEnabled = false
    private(set) var isPresented = false
    private(set) var isFetching = false
    private(set) var snapshot: NativeClipboardSnapshot?
    private(set) var isEditing = false
    var draft = ""
    private(set) var unavailableMessage: String?
    private(set) var notice: String?
    private(set) var pendingRequestID: String?

    @ObservationIgnored
    private let pasteboard: NSPasteboard
    @ObservationIgnored
    private let startsPolling: Bool
    @ObservationIgnored
    private let fetchTimeout: Duration
    @ObservationIgnored
    private let makeRequestID: () -> String
    @ObservationIgnored
    private var timer: Timer?
    @ObservationIgnored
    private var fetchDeadline: Task<Void, Never>?
    @ObservationIgnored
    private var noticeDeadline: Task<Void, Never>?
    @ObservationIgnored
    private var observedChangeCount = 0
    @ObservationIgnored
    private var lastFromRemote: String?
    @ObservationIgnored
    private var lastToRemote: String?

    init(
        pasteboard: NSPasteboard = .general,
        startsPolling: Bool = true,
        fetchTimeout: Duration = .seconds(5),
        makeRequestID: @escaping () -> String = { UUID().uuidString.lowercased() }
    ) {
        self.pasteboard = pasteboard
        self.startsPolling = startsPolling
        self.fetchTimeout = fetchTimeout
        self.makeRequestID = makeRequestID
    }

    isolated deinit {
        timer?.invalidate()
        fetchDeadline?.cancel()
        noticeDeadline?.cancel()
    }

    var draftByteCount: Int {
        Self.utf8ByteCount(draft)
    }

    var isOverByteLimit: Bool {
        draftByteCount > Self.maximumBytes
    }

    /// The size of a remote clipboard that was refused for exceeding
    /// ``maximumBytes``, or nil when there is a real snapshot to describe.
    var oversizedBytes: Int64? {
        snapshot?.oversizedBytes
    }

    var concealedCRC32: String? {
        snapshot.map { Self.crc32Hex($0.text) }
    }

    var concealedByteCount: Int? {
        snapshot.map { Self.utf8ByteCount($0.text) }
    }

    var activityDescription: String? {
        guard let snapshot else {
            return nil
        }
        guard let changedAtMs = snapshot.changedAtMs else {
            return "UNKNOWN"
        }
        return Date(timeIntervalSince1970: Double(changedAtMs) / 1_000)
            .formatted(date: .numeric, time: .standard)
    }

    func update(enabled: Bool) {
        guard isEnabled != enabled else {
            return
        }
        isEnabled = enabled
        if enabled {
            observedChangeCount = pasteboard.changeCount
            pushLocalClipboard(force: false)
            guard startsPolling else {
                return
            }
            let timer = Timer(timeInterval: 0.4, repeats: true) {
                [weak self] _ in
                MainActor.assumeIsolated {
                    self?.pollPasteboard()
                }
            }
            RunLoop.main.add(timer, forMode: .common)
            self.timer = timer
        } else {
            timer?.invalidate()
            timer = nil
            lastFromRemote = nil
            lastToRemote = nil
            closePanel()
        }
    }

    /// Only unsolicited remote activity reaches this path. A requested fetch
    /// is panel state and must never cross the NSPasteboard consent boundary.
    func receiveRemotePush(_ text: String) {
        guard isEnabled, !text.isEmpty else {
            return
        }
        let alreadyMirrored = text == lastFromRemote
        let echoedFromViewer = text == lastToRemote
        lastFromRemote = text
        guard !alreadyMirrored, !echoedFromViewer else {
            return
        }

        // A local owner changed the pasteboard after our last observation.
        // Push that newer value and leave it intact instead of allowing an
        // older remote notification to overwrite it.
        guard pasteboard.changeCount == observedChangeCount else {
            pollPasteboard()
            return
        }

        pasteboard.clearContents()
        guard pasteboard.setString(text, forType: .string) else {
            showNotice("Could not copy remote clipboard")
            observedChangeCount = pasteboard.changeCount
            return
        }
        observedChangeCount = pasteboard.changeCount
    }

    /// The remote copied more than can be transferred. Nothing reaches the
    /// pasteboard — there is nothing to put on it — but an open panel showing
    /// the previous value would now be describing a clipboard that has moved on,
    /// so it is updated to say what happened.
    func noteRemoteOversized(bytes: Int64) {
        guard isEnabled else {
            return
        }
        // Cleared so the echo guard cannot mistake the next arrival of the old
        // text for something already mirrored.
        lastFromRemote = nil
        guard isPresented, !isEditing else {
            return
        }
        snapshot = NativeClipboardSnapshot(
            text: "",
            changedAtMs: nil,
            oversizedBytes: bytes
        )
    }

    /// An oversized pasteboard is skipped rather than sent for the gateway to
    /// truncate. Nothing here is user-initiated — polling and the Command-V hook
    /// both land in this path — and the whole string would ride the socket
    /// first, which past 64 MiB drops the session outright. The remote keeps
    /// whatever it had; `sendDraft` is the path that reports the limit, because
    /// it has the card to report it on.
    ///
    /// `receiveRemotePush` needs no such check: the gateway has already clamped
    /// everything arriving on that link.
    func pushLocalClipboard(force: Bool) {
        guard isEnabled,
              let text = pasteboard.string(forType: .string),
              !text.isEmpty,
              Self.utf8ByteCount(text) <= Self.maximumBytes
        else {
            return
        }
        guard force || (text != lastFromRemote && text != lastToRemote) else {
            return
        }
        lastToRemote = text
        send?(.clipboard(text: text))
    }

    func pollPasteboard() {
        guard isEnabled, pasteboard.changeCount != observedChangeCount else {
            return
        }
        observedChangeCount = pasteboard.changeCount
        pushLocalClipboard(force: false)
    }

    func togglePanel() {
        if isPresented {
            closePanel()
        } else {
            requestFreshSnapshot()
        }
    }

    func requestFreshSnapshot() {
        guard isEnabled, !isFetching else {
            return
        }
        resetPanelContent()
        // The request id is this object's own, not the wire's — `clipboardRequest`
        // carries none, and the gateway answers with a single `clipboard` reply
        // marked `requested`. Keeping the id means an answer that arrives after a
        // close or a second Fetch still cannot land in the wrong panel.
        let requestID = makeRequestID()
        pendingRequestID = requestID
        isFetching = true
        send?(.clipboardRequest)
        fetchDeadline?.cancel()
        fetchDeadline = Task { [weak self] in
            guard let self else {
                return
            }
            do {
                try await Task.sleep(for: self.fetchTimeout)
            } catch {
                return
            }
            guard !Task.isCancelled else {
                return
            }
            self.fetchUnavailable(requestID: requestID)
        }
    }

    @discardableResult
    func receiveFetchResult(
        requestID: String,
        text: String,
        changedAtMs: Int64?,
        oversizedBytes: Int64? = nil
    ) -> Bool {
        guard isEnabled, isFetching, requestID == pendingRequestID else {
            return false
        }
        fetchDeadline?.cancel()
        fetchDeadline = nil
        pendingRequestID = nil
        isFetching = false
        snapshot = NativeClipboardSnapshot(
            text: text,
            changedAtMs: changedAtMs,
            oversizedBytes: oversizedBytes
        )
        isEditing = false
        draft = ""
        unavailableMessage = nil
        notice = nil
        isPresented = true
        return true
    }

    /// The gateway's answer to a `clipboardRequest` — a `clipboard` message with
    /// `requested` set. Correlated against the fetch in flight, since the wire
    /// carries no request id of its own.
    @discardableResult
    func receiveFetchReply(
        text: String,
        changedAtMs: Int64?,
        oversizedBytes: Int64?
    ) -> Bool {
        guard let pendingRequestID else {
            return false
        }
        return receiveFetchResult(
            requestID: pendingRequestID,
            text: text,
            changedAtMs: changedAtMs,
            oversizedBytes: oversizedBytes
        )
    }

    /// Give up on the fetch in flight, if there is one. Called when the socket
    /// goes away or the session returns to the picker: nothing is left to answer,
    /// so the button should not sit on "Fetching…" until its own deadline.
    func failPendingFetch() {
        guard let pendingRequestID else {
            return
        }
        fetchUnavailable(requestID: pendingRequestID)
    }

    /// Either the local deadline expired or there is nothing left that could
    /// answer. Both leave the panel open on an empty editor, so the draft is
    /// still sendable.
    func fetchUnavailable(requestID: String) {
        guard isEnabled, isFetching, requestID == pendingRequestID else {
            return
        }
        fetchDeadline?.cancel()
        fetchDeadline = nil
        pendingRequestID = nil
        isFetching = false
        snapshot = nil
        isEditing = true
        draft = ""
        unavailableMessage = "Remote clipboard unavailable"
        notice = nil
        isPresented = true
    }

    func reveal() {
        guard let snapshot else {
            return
        }
        // Nothing was transferred, so there is nothing to reveal. Still opens
        // the editor: it is the only way on from here, and Send is then usable
        // for text typed in by hand.
        guard snapshot.oversizedBytes == nil else {
            self.snapshot = nil
            draft = ""
            isEditing = true
            unavailableMessage = "Remote clipboard was too large to transfer"
            notice = nil
            return
        }
        draft = snapshot.text
        isEditing = true
        unavailableMessage = nil
        notice = nil
    }

    @discardableResult
    func copy() -> Bool {
        let text = isEditing ? draft : (snapshot?.text ?? "")
        // Copying nothing would still clear the local pasteboard, which is a
        // destructive answer to a button the panel offers in every state —
        // including the unavailable editor, whose draft starts empty. The two
        // reasons there is nothing read the same from here, so they are named
        // apart: one of them means the remote copied nothing.
        guard !text.isEmpty else {
            showNotice(
                oversizedBytes == nil
                    ? "Nothing to copy"
                    : "Remote clipboard too large to transfer"
            )
            return false
        }
        pasteboard.clearContents()
        let copied = pasteboard.setString(text, forType: .string)
        observedChangeCount = pasteboard.changeCount
        showNotice(copied ? "Clipboard copied" : "Could not copy clipboard")
        return copied
    }

    @discardableResult
    func sendDraft() -> Bool {
        // An empty draft is not a way to clear the remote clipboard: the remote
        // takes ownership of whatever arrives, so sending nothing would wipe it
        // while reporting success. `pushLocalClipboard` skips empty text for the
        // same reason.
        guard isEnabled, isPresented, isEditing, !draft.isEmpty else {
            showNotice("Reveal or enter clipboard text first")
            return false
        }
        guard !isOverByteLimit else {
            showNotice("Clipboard exceeds the 65,536-byte limit")
            return false
        }
        lastToRemote = draft
        send?(.clipboard(text: draft))
        showNotice("Clipboard sent to remote")
        return true
    }

    func closePanel() {
        fetchDeadline?.cancel()
        fetchDeadline = nil
        pendingRequestID = nil
        isFetching = false
        isPresented = false
        resetPanelContent()
    }

    static func utf8ByteCount(_ text: String) -> Int {
        text.utf8.count
    }

    static func crc32Hex(_ text: String) -> String {
        var crc: UInt32 = 0xffff_ffff
        for byte in text.utf8 {
            var value = (crc ^ UInt32(byte)) & 0xff
            for _ in 0..<8 {
                value = value & 1 == 1
                    ? 0xedb8_8320 ^ (value >> 1)
                    : value >> 1
            }
            crc = (crc >> 8) ^ value
        }
        return String(format: "%08x", crc ^ 0xffff_ffff)
    }

    private func resetPanelContent() {
        noticeDeadline?.cancel()
        noticeDeadline = nil
        snapshot = nil
        isEditing = false
        draft = ""
        unavailableMessage = nil
        notice = nil
    }

    private func showNotice(_ message: String) {
        notice = message
        noticeDeadline?.cancel()
        noticeDeadline = Task { [weak self] in
            do {
                try await Task.sleep(for: .milliseconds(1_800))
            } catch {
                return
            }
            guard !Task.isCancelled, self?.notice == message else {
                return
            }
            self?.notice = nil
        }
    }
}
