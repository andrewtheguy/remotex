import AVFoundation
import Observation
import OSLog

/// Main-actor owner of remote audio subscription, decode, and playback.
///
/// Explicit scheduling bounds latency. Keeping decode and scheduling on the main
/// actor preserves packet order without making the socket receive loop await the
/// audio engine.
@MainActor
@Observable
final class AudioOutput {
    struct Device {
        let engine: AVAudioEngine
        let player: AVAudioPlayerNode
    }

    /// Returns `nil` for a deliberate silent sink. Tests use that to exercise stream
    /// setup without asking the machine for an output device.
    private let startDevice: @MainActor (AVAudioFormat) throws -> Device?

    /// Whether the user has asked for sound — intent, not whether any is arriving.
    ///
    /// Nothing here reports the latter, for the same reason the SPA does not: from this
    /// end a quiet remote and one whose audio channel will never come up are the same
    /// thing. The gateway's log is where they differ.
    private(set) var isEnabled = false

    /// Whether the toggle can be used at all: a live desktop whose target carries sound.
    private(set) var isAvailable = false

    /// Set by `AppModel` for as long as a session is attached.
    @ObservationIgnored
    var send: (@MainActor (ClientMessage) -> Void)?

    /// One-shot error reporting avoids stale observable state across sessions.
    @ObservationIgnored
    var report: (@MainActor (String) -> Void)?

    @ObservationIgnored
    private var engine: AVAudioEngine?
    @ObservationIgnored
    private var player: AVAudioPlayerNode?
    @ObservationIgnored
    private var decoder: OpusDecoder?
    /// Timeline position on the player clock; stopping the node rebases it.
    @ObservationIgnored
    private var nextAt = 0.0
    @ObservationIgnored
    private var configurationObserver: (any NSObjectProtocol)?
    @ObservationIgnored
    private let log = Logger(subsystem: "dev.remotex.viewer", category: "audio")

    init(
        startDevice: @escaping @MainActor (AVAudioFormat) throws -> Device? = AudioOutput.startSystemDevice
    ) {
        self.startDevice = startDevice
    }

    /// `isolated` so it can reach main-actor state, as `ClipboardSynchronizer`'s does.
    isolated deinit {
        teardown()
    }

    // MARK: - The control

    /// The Remote menu's toggle.
    ///
    /// Sending the message is all this does in the "on" direction: the gateway answers
    /// with an `audioFormat`, and that is what builds a decoder — so there is nothing to
    /// set up until it arrives, and nothing to undo if it never does.
    func setEnabled(_ enabled: Bool) {
        guard enabled != isEnabled else {
            return
        }
        isEnabled = enabled
        if !enabled {
            teardown()
        }
        send?(.audio(enabled: enabled))
    }

    /// Called as the session moves: a desktop, connected, on a target that carries
    /// sound.
    ///
    /// Losing availability stops the audio but **keeps** `isEnabled`, so a target switch
    /// is what clears intent (`AppModel`, on `picker`) rather than the momentary
    /// disconnection in the middle of a reconnect.
    func update(available: Bool) {
        isAvailable = available
        if !available {
            teardown()
        }
    }

    /// Forget the user's answer. For a target switch, where the answer was about the
    /// target being left.
    func reset() {
        isEnabled = false
        isAvailable = false
        teardown()
    }

    /// Re-subscribe on a fresh attachment.
    ///
    /// The gateway's subscription belongs to an *attachment*, so a reconnect arrives
    /// with audio off while this side still says on. Re-sent from `connected` for the
    /// same reason the viewport and the host scale are: a freshly started engine knows
    /// nothing about this client.
    func reassert() {
        guard isEnabled else {
            return
        }
        // The old stream's decoder and timeline belong to the attachment that ended.
        teardown()
        send?(.audio(enabled: true))
    }

    // MARK: - The stream

    /// A new stream is starting: build a decoder and an engine for it.
    func start(format: ServerMessage.AudioFormat) {
        teardown()
        guard isEnabled else {
            // Audio arriving for an attachment that has since turned it off. The
            // gateway stops on its own; there is nothing to play into.
            return
        }
        guard let decoder = OpusDecoder(format: format) else {
            log.error("no decoder for codec \(format.codec, privacy: .public)")
            report?("This Mac cannot decode the audio the gateway is sending (\(format.codec)).")
            return
        }
        self.decoder = decoder
        do {
            try startEngine(format: decoder.format)
        } catch {
            log.error("audio engine: \(error.localizedDescription, privacy: .public)")
            report?("The audio device could not be started.")
            self.decoder = nil
            return
        }
        log.info(
            """
            audio: \(format.codec, privacy: .public) \
            \(Int(format.sampleRate), privacy: .public) Hz \
            \(format.channels, privacy: .public)ch
            """
        )
    }

    /// One wave buffer's packets, decoded and placed on the timeline.
    func play(packets: [Data]) {
        guard let decoder, let player, let buffer = decoder.decode(packets) else {
            return
        }
        let now = currentTime()
        let placed = AudioSchedule.place(
            nextAt: nextAt,
            now: now,
            duration: Double(buffer.frameLength) / buffer.format.sampleRate
        )
        if placed.flush {
            // The link is delivering faster than real time. Everything queued is
            // latency, so it goes, and the timeline restarts at the cushion — see
            // `AudioSchedule.place` for why this is coarser than the browser's trim and
            // why coarser is the point.
            let lead = Int((self.nextAt - now) * 1000)
            log.notice("audio: \(lead, privacy: .public) ms ahead of live, queue flushed")
            player.stop()
            player.play()
        }
        nextAt = placed.nextAt
        player.scheduleBuffer(
            buffer,
            at: AVAudioTime(
                sampleTime: AVAudioFramePosition(placed.startAt * buffer.format.sampleRate),
                atRate: buffer.format.sampleRate
            ),
            completionCallbackType: .dataPlayedBack
        ) { _ in }
    }

    // MARK: - The engine

    private func startEngine(format: AVAudioFormat) throws {
        guard let device = try startDevice(format) else {
            // A decoder and stream still exist; only their destination is a test sink.
            nextAt = 0
            return
        }
        let engine = device.engine
        let player = device.player
        self.engine = engine
        self.player = player
        nextAt = 0

        installConfigurationObserver(for: engine, format: format)
    }

    private static func startSystemDevice(format: AVAudioFormat) throws -> Device {
        let engine = AVAudioEngine()
        let player = AVAudioPlayerNode()
        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: format)
        engine.prepare()
        try engine.start()
        player.play()
        return Device(engine: engine, player: player)
    }

    private func installConfigurationObserver(for engine: AVAudioEngine, format: AVAudioFormat) {
        // The default output device changing under the engine — headphones, a display's
        // speakers, a Bluetooth link going away. The engine stops itself, and the old
        // timeline means nothing on the new device.
        //
        // Deregistered before re-registering, not only in `teardown`:
        // `restartAfterConfigurationChange` calls straight back into here, so without
        // this the old token is overwritten while still registered — leaking one block
        // per device change, permanently, since `teardown` can only remove the last one.
        removeConfigurationObserver()
        configurationObserver = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange,
            object: engine,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.restartAfterConfigurationChange(format: format)
            }
        }
    }

    private func restartAfterConfigurationChange(format: AVAudioFormat) {
        guard isEnabled, decoder != nil else {
            return
        }
        log.notice("audio: the output device changed; restarting")
        // Torn down and rebuilt rather than restarted in place: the node graph was
        // connected with the old device's format, and a mismatch here is silence with
        // nothing in the log. The decoder survives, because the *stream* has not
        // changed — only where it is going.
        engine?.stop()
        engine = nil
        player = nil
        do {
            try startEngine(format: format)
        } catch {
            log.error("audio restart: \(error.localizedDescription, privacy: .public)")
            report?("The audio device could not be restarted.")
        }
    }

    /// The player's own clock, which is what every scheduled time is measured against.
    ///
    /// Zero before the first render, and after every `stop()`: an `AVAudioPlayerNode`
    /// timeline restarts rather than continuing the node's. Reading it through
    /// `playerTime(forNodeTime:)` is what keeps this arithmetic in the same frame of
    /// reference as `scheduleBuffer(at:)`.
    private func currentTime() -> Double {
        guard let player,
              let nodeTime = player.lastRenderTime,
              let playerTime = player.playerTime(forNodeTime: nodeTime)
        else {
            return 0
        }
        return Double(playerTime.sampleTime) / playerTime.sampleRate
    }

    private func removeConfigurationObserver() {
        if let configurationObserver {
            NotificationCenter.default.removeObserver(configurationObserver)
            self.configurationObserver = nil
        }
    }

    private func teardown() {
        removeConfigurationObserver()
        player?.stop()
        engine?.stop()
        player = nil
        engine = nil
        decoder = nil
        nextAt = 0
    }
}
