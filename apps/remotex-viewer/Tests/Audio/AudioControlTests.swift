import Foundation
import Testing
@testable import RemotexViewer

/// The Remote menu's audio toggle and what it puts on the wire, over a scripted socket.
///
/// Nothing here plays anything, and nothing here could: playback is the canvas page's.
/// What is testable on this side is the *subscription* — when the item is live, what is
/// sent, and what the session's own transitions do to the answer. The parts that are not
/// are covered where the code is: the arithmetic in `audioSchedule.test.ts`, the decoding
/// in the canvas page's WebCodecs path, and the sound itself by ear against the tone
/// harness.
@MainActor
struct AudioControlTests {
    /// Availability is the target's answer, not a preference: the item is greyed until a
    /// `connected` says this session carries sound.
    @Test
    func theToggleIsLiveOnlyForATargetThatCarriesSound() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        #expect(!session.model.audio.isAvailable, "the picker has no audio to enable")

        session.connect(protocolName: "rdp", audio: false)
        #expect(!session.model.audio.isAvailable, "a target without audio leaves it greyed")

        session.connect(protocolName: "rdp", audio: true)
        #expect(session.model.audio.isAvailable)
    }

    @Test
    func enablingSubscribesAndDisablingUnsubscribes() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)

        session.model.audio.setEnabled(true)
        try await session.settle()
        #expect(session.audioSocketsOpened == 1)
        #expect(session.isSubscribedToAudio)

        session.model.audio.setEnabled(false)
        try await session.settle()
        #expect(session.audioSocketsOpened == 1, "unsubscribing closes, it does not reopen")
        #expect(!session.isSubscribedToAudio)
    }

    /// Pressing an already-on toggle must not re-subscribe. A second socket supersedes
    /// the first at the gateway, so the cost is a fresh encoder and a fresh
    /// `audioFormat` mid-stream — a gap in the sound for no reason.
    @Test
    func askingTwiceForTheSameAnswerOpensOneSocket() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)

        session.model.audio.setEnabled(true)
        session.model.audio.setEnabled(true)
        try await session.settle()
        #expect(session.audioSocketsOpened == 1)
    }

    /// The gateway keeps its half across a reattach — the subscription belongs to the
    /// claim — but this end's socket died with the network, so the viewer reopens it.
    /// A deliberate difference from the SPA, where a reconnect needs a fresh click,
    /// because a browser's audio context needs a user gesture and this does not.
    @Test
    func aReconnectResubscribesWithoutBeingAskedAgain() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)
        session.model.audio.setEnabled(true)
        try await session.settle()

        // What a reattach looks like from here: the same target's `connected` again.
        session.connect(protocolName: "rdp", audio: true)
        try await session.settle()
        #expect(session.audioSocketsOpened == 2)
        #expect(session.isSubscribedToAudio)
        #expect(session.model.audio.isEnabled)
    }

    /// A target switch clears the answer, because it was an answer about the target
    /// being left — the same rule the resize mode follows, and the picker is where
    /// both are forgotten.
    @Test
    func aTargetSwitchForgetsTheAnswer() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)
        session.model.audio.setEnabled(true)
        try await session.settle()

        session.model.apply(.control(.picker))
        #expect(!session.model.audio.isEnabled)
        #expect(!session.model.audio.isAvailable)
        #expect(!session.model.session.canAudio)

        // And the next target is asked about on its own terms.
        session.connect(protocolName: "rdp", audio: true)
        try await session.settle()
        #expect(!session.model.audio.isEnabled, "the new target starts silent")
        #expect(session.audioSocketsOpened == 1, "nothing was re-asserted for a fresh target")
        #expect(!session.isSubscribedToAudio)
    }

    /// A reconnection in progress greys the item — there is no attachment to subscribe
    /// on — but must not answer for the user, because the reconnect is expected to come
    /// back and re-assert.
    @Test
    func losingTheConnectionGreysTheItemWithoutForgettingTheAnswer() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)
        session.model.audio.setEnabled(true)

        session.model.apply(.status(.reconnecting))
        #expect(!session.model.audio.isAvailable)
        #expect(session.model.audio.isEnabled, "the answer outlives the socket")
    }

    /// The format is handed to the page whole, including the `OpusHead` a decoder
    /// needs for the pre-skip. Nothing here reads the codec: this side has no
    /// decoder to choose and would only be a second opinion about one.
    @Test
    func theFormatIsHandedToThePageVerbatim() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)
        session.model.audio.setEnabled(true)

        // The gateway's own 19-byte OpusHead, as protocol.rs pins it. It reaches
        // the page as base64 again, which is `JSONEncoder`'s own `Data` encoding
        // and what `decodeAudioHead` reads.
        let head = try #require(Data(base64Encoded: "T3B1c0hlYWQBAjgBRKwAAAAAAA=="))
        let format = ServerMessage.AudioFormat(
            codec: "opus",
            sampleRate: 48_000,
            channels: 2,
            packetFrames: 960,
            head: head
        )
        session.model.apply(.control(.audioFormat(format)))

        #expect(session.canvas.commands.contains(.audioFormat(format)))
        #expect(session.model.actionError == nil, "the ordinary path says nothing")
    }

    /// A page comes back — a reload, or a stream that dropped and reattached —
    /// and has never heard an `audioFormat`, because the gateway sends one when
    /// a subscription starts and never again. Without re-subscribing it would
    /// receive packets it has no decoder for and be silent with nothing to say
    /// why, which is the failure this whole path is shaped around.
    @Test
    func areattachedPageIsResubscribedSoTheFormatComesAgain() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)
        session.model.audio.setEnabled(true)
        try await session.settle()
        #expect(session.audioSocketsOpened == 1)

        session.model.attach(canvas: FakeCanvas())
        try await session.settle()
        #expect(session.audioSocketsOpened == 2, "the reattached page gets a format")
        #expect(session.model.audio.isEnabled, "and the answer it was playing under")
    }

    /// The same reattachment costs a silent session nothing. `reassert` is a
    /// no-op when sound was never asked for, so a target with no audio — or one
    /// the user muted — does not subscribe itself by reloading a page.
    @Test
    func areattachedPageDoesNotSubscribeSoundNobodyAskedFor() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)
        try await session.settle()

        session.model.attach(canvas: FakeCanvas())
        try await session.settle()
        #expect(session.audioSocketsOpened == 0)
    }

    /// A failure reported for a subscription that is already off is not the
    /// user's problem: the decoder giving up on its way out is the ordinary end
    /// of a stream, and an alert about sound nobody asked for any more describes
    /// nothing that is wrong.
    @Test
    func aFailureAfterTheToggleWentOffIsNotAnAlert() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)
        #expect(!session.model.audio.isEnabled)

        session.model.audio.playbackFailed("decoder closed")
        try await session.settle()

        #expect(session.model.actionError == nil)
        #expect(session.audioSocketsOpened == 0, "nothing to unsubscribe from")
    }

    /// A page that cannot play what arrived says so, and the alert is how that
    /// reaches anyone: a reason nothing displays is the same as no reason at all.
    /// The subscription goes with it, because packets decoded by nothing are bytes
    /// spent on nothing — the same move `useRemoteDesktop.ts` makes on `onError`.
    @Test
    func aPageThatCannotPlayReportsItAndUnsubscribes() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)
        session.model.audio.setEnabled(true)
        #expect(session.model.actionError == nil)

        session.model.audio.playbackFailed("this browser cannot decode vorbis")
        try await session.settle()

        let reported = try #require(session.model.actionError)
        #expect(reported.contains("vorbis"), "the alert should name what arrived: \(reported)")
        #expect(!session.model.audio.isEnabled)
        #expect(
            !session.isSubscribedToAudio,
            "the socket closes, so the gateway stops sending what nothing will decode"
        )
    }
}

