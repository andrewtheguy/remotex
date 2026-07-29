import AVFoundation
import OSLog

/// The remote's Opus packets, turned into PCM by the decoder macOS already has.
///
/// **No dependency and no container.** macOS ships a real Opus decoder —
/// `/System/Library/Components/AudioCodecs.component` exports `ACOpusDecoderFactory`
/// and calls `opus_decoder_create` — and `AVAudioConverter` reaches it through
/// `kAudioFormatOpus`. It accepts the wire's **bare** packets described by nothing but
/// an `AudioStreamBasicDescription`, which is what makes this possible at all: without
/// that, viewer audio would have meant either vendoring libopus into this package or
/// wrapping every wave buffer in a CAF for the system's parser to unwrap again.
///
/// That was measured before it was designed on (2026-07-29, against fixtures from both
/// `opusenc` and this gateway's own encoder), along with the two facts below that this
/// file exists to handle. It is worth knowing they were measured rather than read: none
/// of it is documented, and `kAudioFormatOpus` existing says nothing about whether the
/// converter's front end will take packets without a container around them.
///
/// **The magic cookie is ignored.** Setting `converter.magicCookie` to the `OpusHead`
/// reads back `nil` and changes no output byte — the decoder takes its rate and channel
/// count from the ASBD, which is all libopus needs. So one is deliberately not set, and
/// the `OpusHead` is used here for the one thing it still carries: the pre-skip.
final class OpusDecoder {
    /// libopus encodes at 48 kHz and nothing else, so this is the rate the packets are
    /// in whatever the remote negotiated — the gateway resamples on the way in
    /// (`src/opus_stream.rs`).
    static let sampleRate = 48_000.0
    /// 20 ms at 48 kHz, the frame the gateway cuts (`FRAME_FRAMES`).
    ///
    /// Every packet on this wire is one, so it can be declared in the ASBD instead of
    /// left variable — which is what lets a whole wave buffer be converted in one call.
    static let framesPerPacket: UInt32 = 960

    /// What the player node is connected with, so the common case resamples nothing.
    let format: AVAudioFormat

    private let converter: AVAudioConverter
    /// The encoder's own delay, from the `OpusHead`: audio the stream was never meant
    /// to contain. 312 frames in practice.
    private let preSkip: Int
    /// How much of `preSkip` is still to be discarded.
    ///
    /// Counted down rather than applied as a constant to the first buffer, because the
    /// converter swallows some of it itself: its **first** call returns 120 frames fewer
    /// than its packets hold, and every call after that returns exactly its own — a
    /// one-off priming trim, not a per-call loss (a per-call loss would have been a skew
    /// growing all session, which is why it was worth measuring rather than assuming).
    /// 120 is not 312, so the decoder is plainly not honouring the pre-skip itself, and
    /// what is left has to go. Deriving it from what actually came back means the
    /// priming figure is CoreAudio's business and may change without this being wrong.
    private var preSkipRemaining: Int
    private let log = Logger(subsystem: "dev.remotex.viewer", category: "audio")

    /// Nil if the gateway described something this cannot decode.
    ///
    /// The codec is checked rather than assumed: there is one on this wire and no
    /// fallback representation in either direction, so a gateway that grew a second one
    /// should be refused here — audibly, as no sound and a line in the log — rather than
    /// have its packets fed to a decoder built for Opus.
    init?(format: ServerMessage.AudioFormat) {
        guard format.codec == "opus" else {
            return nil
        }
        var asbd = AudioStreamBasicDescription(
            mSampleRate: format.sampleRate,
            mFormatID: kAudioFormatOpus,
            mFormatFlags: 0,
            // Variable: an Opus packet does not have a fixed size, which is also why the
            // wire length-prefixes them.
            mBytesPerPacket: 0,
            mFramesPerPacket: Self.framesPerPacket,
            mBytesPerFrame: 0,
            mChannelsPerFrame: format.channels,
            mBitsPerChannel: 0,
            mReserved: 0
        )
        guard let compressed = AVAudioFormat(streamDescription: &asbd),
              let pcm = AVAudioFormat(
                  commonFormat: .pcmFormatFloat32,
                  sampleRate: format.sampleRate,
                  channels: AVAudioChannelCount(format.channels),
                  // Deinterleaved, for the same reason the gateway's resampler works
                  // that way: anything treating interleaved samples as one signal
                  // blends left into right.
                  interleaved: false
              ),
              let converter = AVAudioConverter(from: compressed, to: pcm)
        else {
            return nil
        }
        self.format = pcm
        self.converter = converter
        // Bytes 10–11 of the OpusHead, little-endian (RFC 7845 §5.1). A head too short
        // to hold one is not something the gateway sends; nothing is discarded then,
        // which costs 6.5 ms of near-silence rather than a refusal to play.
        if format.head.count >= 12 {
            let bytes = Array(format.head)
            preSkip = Int(UInt16(bytes[10]) | (UInt16(bytes[11]) << 8))
        } else {
            preSkip = 0
        }
        preSkipRemaining = preSkip
    }

    /// One wave buffer's packets as PCM, or nil if the converter refused them.
    ///
    /// Converted in one call rather than one per packet: a live host's frame holds nine
    /// or ten, the tone harness's holds one, and the converter is happy to take a whole
    /// frame's worth at once.
    func decode(_ packets: [Data]) -> AVAudioPCMBuffer? {
        guard !packets.isEmpty, let input = compressedBuffer(packets).map(Supply.init) else {
            return nil
        }
        let expected = AVAudioFrameCount(UInt32(packets.count) * Self.framesPerPacket)
        // Room for more than this frame's own audio: the converter may return frames it
        // held back from a previous call.
        guard let output = AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: expected + Self.framesPerPacket
        ) else {
            return nil
        }

        // `nonisolated(unsafe)`, and `Supply` above, for one reason:
        // `AVAudioConverterInputBlock` is declared `@Sendable` but is called
        // *synchronously* — `convert(to:error:withInputFrom:)` invokes it on this thread
        // before returning — so there is no concurrency here for the compiler to protect
        // against, only a signature that cannot say so. Scoped to these two rather than
        // silenced for the file with `@preconcurrency import`, which would also hide the
        // next Sendable diagnostic from AVFAudio, and that one might be real.
        nonisolated(unsafe) var supplied = false
        var error: NSError?
        let status = converter.convert(to: output, error: &error) { _, outStatus in
            if supplied {
                // Everything this call was given has been taken. Not `.endOfStream`:
                // that would tear the converter down between wave buffers, and the
                // stream continues — Opus packets carry inter-packet state.
                outStatus.pointee = .noDataNow
                return nil
            }
            supplied = true
            outStatus.pointee = .haveData
            return input.buffer
        }
        // `.inputRanDry` is the ordinary answer here, since one frame's packets are all
        // there is until the next frame arrives.
        guard status != .error else {
            log.warning(
                "opus decode failed: \(error?.localizedDescription ?? "unknown", privacy: .public)"
            )
            return nil
        }
        guard output.frameLength > 0 else {
            return nil
        }
        return discardingPreSkip(output, expected: expected)
    }

    /// Drop whatever is left of the encoder's delay off the front.
    ///
    /// The frames the converter kept for itself count toward it, which is what keeps
    /// this from over-discarding by the priming amount.
    private func discardingPreSkip(
        _ buffer: AVAudioPCMBuffer,
        expected: AVAudioFrameCount
    ) -> AVAudioPCMBuffer? {
        guard preSkipRemaining > 0 else {
            return buffer
        }
        preSkipRemaining -= max(0, Int(expected) - Int(buffer.frameLength))
        guard preSkipRemaining > 0 else {
            return buffer
        }
        let drop = min(preSkipRemaining, Int(buffer.frameLength))
        preSkipRemaining -= drop
        guard drop < Int(buffer.frameLength) else {
            // The whole buffer was the encoder's delay. Only reachable for a stream
            // whose first frame is shorter than its pre-skip, which the tone harness's
            // single-packet frames very nearly are.
            return nil
        }
        return buffer.dropping(frames: AVAudioFrameCount(drop))
    }

    private func compressedBuffer(_ packets: [Data]) -> AVAudioCompressedBuffer? {
        guard let maximum = packets.map(\.count).max() else {
            return nil
        }
        let buffer = AVAudioCompressedBuffer(
            format: converter.inputFormat,
            packetCapacity: AVAudioPacketCount(packets.count),
            maximumPacketSize: maximum
        )
        guard let descriptions = buffer.packetDescriptions else {
            return nil
        }
        var offset = 0
        for (index, packet) in packets.enumerated() {
            packet.withUnsafeBytes { raw in
                guard let base = raw.baseAddress else {
                    return
                }
                buffer.data.advanced(by: offset).copyMemory(from: base, byteCount: packet.count)
            }
            descriptions[index] = AudioStreamPacketDescription(
                mStartOffset: Int64(offset),
                // Only meaningful for formats whose packets hold a variable number of
                // frames; every packet here holds `framesPerPacket`.
                mVariableFramesInPacket: 0,
                mDataByteSize: UInt32(packet.count)
            )
            offset += packet.count
        }
        buffer.byteLength = UInt32(offset)
        buffer.packetCount = AVAudioPacketCount(packets.count)
        return buffer
    }
}

/// One wave buffer on its way into the converter.
///
/// `@unchecked Sendable` around a buffer that never leaves the `decode` call that made
/// it: it is handed to a synchronous block and dropped. See the note in `decode`.
private struct Supply: @unchecked Sendable {
    let buffer: AVAudioCompressedBuffer
}

extension AVAudioPCMBuffer {
    /// A copy without its first `frames`, which is how the pre-skip is discarded.
    ///
    /// A copy rather than an offset view because an `AVAudioPCMBuffer` has no notion of
    /// a start offset, and this happens once per stream — where the *catch-up* skip,
    /// which happens under load, needs no copy at all: it drops whole buffers.
    func dropping(frames: AVAudioFrameCount) -> AVAudioPCMBuffer? {
        guard frames > 0, frames < frameLength else {
            return frames == 0 ? self : nil
        }
        let remaining = frameLength - frames
        guard let trimmed = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: remaining),
              let source = floatChannelData,
              let destination = trimmed.floatChannelData
        else {
            return nil
        }
        trimmed.frameLength = remaining
        for channel in 0 ..< Int(format.channelCount) {
            destination[channel].update(
                from: source[channel].advanced(by: Int(frames)),
                count: Int(remaining)
            )
        }
        return trimmed
    }
}
