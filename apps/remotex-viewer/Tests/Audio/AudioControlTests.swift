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
        #expect(session.audioMessages == [true])

        session.model.audio.setEnabled(false)
        try await session.settle()
        #expect(session.audioMessages == [true, false])
    }

    /// Pressing an already-on toggle must not re-subscribe. The gateway replaces the
    /// subscription rather than adding one, so the cost is a fresh encoder and a fresh
    /// `audioFormat` mid-stream — a gap in the sound for no reason.
    @Test
    func askingTwiceForTheSameAnswerSendsNothing() async throws {
        let session = try await AttachedSession.attached(suite: "AudioControlTests")
        session.connect(protocolName: "rdp", audio: true)

        session.model.audio.setEnabled(true)
        session.model.audio.setEnabled(true)
        try await session.settle()
        #expect(session.audioMessages == [true])
    }

    /// The gateway's subscription belongs to an attachment, so a reconnect arrives with
    /// audio off while this side still says on. The viewer re-asserts, which is a
    /// deliberate difference from the SPA — there a reconnect needs a fresh click,
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
        #expect(session.audioMessages == [true, true])
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
        #expect(session.audioMessages == [true], "nothing was re-asserted for a fresh target")
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
            head: head
        )
        session.model.apply(.control(.audioFormat(format)))

        #expect(session.canvas.commands.contains(.audioFormat(format)))
        #expect(session.model.actionError == nil, "the ordinary path says nothing")
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
            session.sent(ofType: "audio").last?["enabled"] as? Bool == false,
            "the gateway is told to stop sending what nothing will decode"
        )
    }
}

extension AttachedSession {
    /// The `enabled` of every `audio` message sent, in order.
    var audioMessages: [Bool] {
        sent.compactMap { frame in
            frame["type"] as? String == "audio" ? frame["enabled"] as? Bool : nil
        }
    }
}
