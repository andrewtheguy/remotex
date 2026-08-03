import AppKit
import Foundation
import Observation

/// The Mac's pasteboard and the remote's clipboard, kept in step.
///
/// This is one of the few things the app still does for itself, and it is here for
/// a reason a web page cannot argue with: reading `NSPasteboard` on a timer and
/// writing it without a user gesture are both things a document is not allowed to
/// do. The page owns the *panel* — the reveal, the copy consent, the editor — and
/// this owns the automatic sync underneath it.
///
/// Both directions are guarded against echoing. The page has guards of its own for
/// the same loop seen from the other side; these ones stop a value that arrived
/// from the remote being handed straight back to it as though the user had copied
/// it here.
@MainActor
@Observable
final class ClipboardSynchronizer {
    /// The gateway refuses anything larger in either direction, so an oversized
    /// value is skipped rather than truncated — the remote keeps what it had.
    static let maximumBytes = 65_536

    /// Where a local clipboard change goes. The bridge's `clipboardLocal` command,
    /// set by the model while a page is up.
    var sendText: ((String) -> Void)?

    private(set) var isEnabled = false

    @ObservationIgnored
    private let pasteboard: NSPasteboard
    @ObservationIgnored
    private let startsPolling: Bool
    @ObservationIgnored
    private var timer: Timer?
    @ObservationIgnored
    private var observedChangeCount = 0
    @ObservationIgnored
    private var lastFromRemote: String?
    @ObservationIgnored
    private var lastToRemote: String?

    /// `startsPolling` is false in tests, which drive `pollPasteboard` themselves
    /// rather than waiting out a timer.
    init(pasteboard: NSPasteboard = .general, startsPolling: Bool = true) {
        self.pasteboard = pasteboard
        self.startsPolling = startsPolling
    }

    isolated deinit {
        timer?.invalidate()
    }

    /// Follow or stop following the pasteboard.
    ///
    /// Enabling snapshots the change count and pushes once: the value on the Mac's
    /// pasteboard when a desktop comes up is one the user may well be about to
    /// paste, and it predates anything this could have observed.
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
            let timer = Timer(timeInterval: 0.4, repeats: true) { [weak self] _ in
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
        }
    }

    /// The remote's clipboard changed on its own. Only unsolicited pushes reach
    /// here: a value the *user* asked to see is the page's panel to show, and its
    /// Copy button is the consent to put it on this Mac.
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

        // Preserve and send a newer local value instead of overwriting it: somebody
        // who copied here a moment ago wants their own text when they paste.
        guard pasteboard.changeCount == observedChangeCount else {
            pollPasteboard()
            return
        }

        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
        observedChangeCount = pasteboard.changeCount
    }

    /// Send this Mac's clipboard, if there is anything worth sending.
    ///
    /// `force` is Command-V's: the paste is about to happen, so the remote is given
    /// the current value whether or not this has already sent it.
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
        sendText?(text)
    }

    func pollPasteboard() {
        guard isEnabled, pasteboard.changeCount != observedChangeCount else {
            return
        }
        observedChangeCount = pasteboard.changeCount
        pushLocalClipboard(force: false)
    }

    static func utf8ByteCount(_ text: String) -> Int {
        text.utf8.count
    }
}
