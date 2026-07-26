import AppKit
import Foundation
import Observation

struct NativeClipboardSnapshot: Equatable {
    let text: String
    let changedAtMs: Int64?
}

@MainActor
@Observable
final class ClipboardSynchronizer {
    static let maximumBytes = 65_536

    var sendCommand: (([String: Any]) -> Void)?

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
    func receiveRemotePush(_ text: String, changedAtMs: Int64?) {
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
        _ = changedAtMs
    }

    func pushLocalClipboard(force: Bool) {
        guard isEnabled,
              let text = pasteboard.string(forType: .string),
              !text.isEmpty
        else {
            return
        }
        guard force || (text != lastFromRemote && text != lastToRemote) else {
            return
        }
        lastToRemote = text
        sendCommand?([
            "type": "clipboard",
            "text": text,
        ])
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
        let requestID = makeRequestID()
        pendingRequestID = requestID
        isFetching = true
        sendCommand?([
            "type": "clipboardRequest",
            "requestId": requestID,
        ])
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
            self.fetchTimedOut(requestID: requestID)
        }
    }

    @discardableResult
    func receiveFetchResult(
        requestID: String,
        text: String,
        changedAtMs: Int64?
    ) -> Bool {
        guard isEnabled, isFetching, requestID == pendingRequestID else {
            return false
        }
        fetchDeadline?.cancel()
        fetchDeadline = nil
        pendingRequestID = nil
        isFetching = false
        snapshot = NativeClipboardSnapshot(text: text, changedAtMs: changedAtMs)
        isEditing = false
        draft = ""
        unavailableMessage = nil
        notice = nil
        isPresented = true
        return true
    }

    func fetchTimedOut(requestID: String) {
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
        draft = snapshot.text
        isEditing = true
        unavailableMessage = nil
        notice = nil
    }

    @discardableResult
    func copy() -> Bool {
        let text = isEditing ? draft : (snapshot?.text ?? "")
        pasteboard.clearContents()
        let copied = pasteboard.setString(text, forType: .string)
        observedChangeCount = pasteboard.changeCount
        showNotice(copied ? "Clipboard copied" : "Could not copy clipboard")
        return copied
    }

    @discardableResult
    func sendDraft() -> Bool {
        guard isEnabled, isPresented, isEditing else {
            showNotice("Reveal or enter clipboard text first")
            return false
        }
        guard !isOverByteLimit else {
            showNotice("Clipboard exceeds the 65,536-byte limit")
            return false
        }
        lastToRemote = draft
        sendCommand?([
            "type": "clipboard",
            "text": draft,
        ])
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
