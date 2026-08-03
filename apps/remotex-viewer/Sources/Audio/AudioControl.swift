import Observation
import OSLog

/// Main-actor owner of the remote audio *subscription*.
///
/// Decode and playback are the canvas page's (`frontend/src/viewer/audio.ts`,
/// over the same WebCodecs path the browser SPA uses); what stays here is the
/// half a menu item needs — whether sound was asked for, whether this target can
/// carry it, and what to say when the page reports it cannot play what arrived.
///
/// The split is not new: even with an `AVAudioEngine` behind it, this type never
/// did more in the "on" direction than ask the gateway for sound, and the gateway's
/// `audioFormat` was what built anything. The engine went to the page; this did not
/// have to move. What it asks *with* has changed — a socket rather than a message —
/// and that did not have to move it either.
@MainActor
@Observable
final class AudioControl {
    /// Whether the user has asked for sound — intent, not whether any is arriving.
    ///
    /// Nothing here reports the latter, for the same reason the SPA does not: from
    /// this end a quiet remote and one whose audio channel will never come up are
    /// the same thing. The gateway's log is where they differ.
    private(set) var isEnabled = false

    /// Whether the toggle can be used at all: a live desktop whose target carries
    /// sound.
    private(set) var isAvailable = false

    /// Open or close the gateway's audio socket. Set by `AppModel` for as long as a
    /// session is attached.
    ///
    /// A callback rather than a `ClientMessage`, because there is no message: opening
    /// `/ws/audio` *is* the subscription and closing it is the only way to stop. What
    /// this type does in the "on" direction is still exactly one thing, which is why
    /// the shape of everything below is unchanged.
    @ObservationIgnored
    var subscribe: (@MainActor (Bool) -> Void)?

    /// One-shot error reporting avoids stale observable state across sessions.
    @ObservationIgnored
    var report: (@MainActor (String) -> Void)?

    /// Tells the page to drop its decoder. Set alongside `subscribe`.
    @ObservationIgnored
    var stopPlayback: (@MainActor () -> Void)?

    @ObservationIgnored
    private let log = Logger(subsystem: "dev.remotex.viewer", category: "audio")

    // MARK: - The control

    /// The Remote menu's toggle.
    ///
    /// Opening the socket is all this does in the "on" direction: the gateway answers
    /// with an `audioFormat`, and that is what builds a decoder — so there is nothing
    /// to set up until it arrives, and nothing to undo if it never does.
    func setEnabled(_ enabled: Bool) {
        guard enabled != isEnabled else {
            return
        }
        isEnabled = enabled
        if !enabled {
            stopPlayback?()
        }
        subscribe?(enabled)
    }

    /// Called as the session moves: a desktop, connected, on a target that carries
    /// sound.
    ///
    /// Losing availability stops the audio but **keeps** `isEnabled`, so a target
    /// switch is what clears intent (`AppModel`, on `picker`) rather than the
    /// momentary disconnection in the middle of a reconnect.
    func update(available: Bool) {
        isAvailable = available
        if !available {
            stopPlayback?()
        }
    }

    /// Forget the user's answer. For a target switch, where the answer was about
    /// the target being left.
    ///
    /// Unsubscribing is part of forgetting, and it did not use to be: the gateway used
    /// to end the subscription with the engine, so clearing the flag here was enough.
    /// It keeps the socket now — that is what lets a target switch resume sound without
    /// asking again — so a switch that means to go silent has to say so, or the next
    /// `connect` arms a stream nobody wants.
    func reset() {
        isEnabled = false
        isAvailable = false
        stopPlayback?()
        subscribe?(false)
    }

    /// Re-subscribe on a fresh attachment.
    ///
    /// The gateway keeps its half across a reattach now — the subscription belongs to
    /// the claim — but this end's socket died with the network, so it is reopened
    /// here. Driven from `connected` for the same reason the viewport and the host
    /// scale are: a freshly started engine knows nothing about this client.
    func reassert() {
        guard isEnabled else {
            return
        }
        // The old stream's decoder and timeline belong to the attachment that ended.
        stopPlayback?()
        subscribe?(true)
    }

    // MARK: - What the page reports back

    /// The page could not play what the gateway sent.
    ///
    /// There is no fallback to switch to, so this unsubscribes rather than
    /// retrying: packets that arrive for a decoder that has gone are bytes spent
    /// on nothing. The same shape as `useRemoteDesktop.ts`'s `onError`.
    /// The alert is raised only for a subscription that was live. A page can
    /// report a failure after the toggle has already gone off — the decoder
    /// giving up on its way out — and an alert about sound nobody asked for any
    /// more is noise about a thing that is not wrong. The log takes every one of
    /// them, because that is where a failure nobody was shown still has to be
    /// findable.
    func playbackFailed(_ reason: String) {
        log.error("canvas audio: \(reason, privacy: .public)")
        guard isEnabled else {
            return
        }
        report?(reason)
        isEnabled = false
        subscribe?(false)
    }
}
