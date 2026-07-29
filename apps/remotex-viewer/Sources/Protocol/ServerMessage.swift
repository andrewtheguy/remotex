import Foundation

/// Gateway -> viewer control messages: `ControlMsg` in `src/protocol.rs`, which
/// is where the exact JSON is pinned by tests worth reading alongside this.
/// Screen tiles are binary frames and decode through `TileFrame` instead.
enum ServerMessage: Sendable, Equatable {
    /// The remote's framebuffer size, and how many of those pixels it draws per
    /// point of its *own* desktop — 1 for VNC, RDP and a 1x Mac, 2 for a Retina
    /// one. The pair is what a client needs to present the desktop at the size it
    /// is meant to be; see `RemoteGeometry`.
    case resize(w: UInt16, h: UInt16, scale: Double)
    case cursor(Cursor)
    /// A fatal engine error. Not a dead end: the session returns to the picker,
    /// so this is shown there.
    case error(message: String)
    case picker
    case connected(Connected)
    case remoteOs(macos: Bool)
    case clipboard(Clipboard)
    /// The remote's displays and which of them is being shared, pushed whenever
    /// either changes. Only `rxa` sends it: RDP and VNC each deliver one
    /// framebuffer spanning every remote screen, so there is nothing to choose
    /// between and the Display menu stays empty.
    case displays(active: UInt32, displays: [DisplayInfo])
    /// How to decode the audio frames that follow, sent once when a subscription
    /// starts and never otherwise.
    ///
    /// It arrives **before** the first packet, on the same ordered channel, because a
    /// decoder configured afterwards has already been handed the audio it was meant to
    /// configure for.
    case audioFormat(AudioFormat)
    /// A `type` this build does not know.
    ///
    /// Held as a value rather than raised as an error so a gateway that adds a
    /// control message cannot break an older viewer: the receive loop must not
    /// stall on it, and it still counts as proof the socket attached to the slot
    /// (which is what resets the reconnect backoff).
    case unsupported(type: String)

    /// The remote pointer shape. Receiving one of these at all means the viewer
    /// owns pointer rendering from then on — engines that composite the pointer
    /// into the framebuffer (RDP, and VNC servers ignoring the Cursor
    /// pseudo-encoding) never send one.
    struct Cursor: Sendable, Equatable, Decodable {
        /// Base64 PNG (RGBA, alpha as the mask). Nil means the remote hid the
        /// pointer, and the other fields are then all zero.
        let image: String?
        let w: UInt16
        let h: UInt16
        /// Hotspot within the image, in cursor *pixels* — not points.
        let hx: UInt16
        let hy: UInt16
    }

    struct Connected: Sendable, Equatable, Decodable {
        let name: String
        /// `"rdp"`, `"vnc"`, or `"rxa"`. Named around the Swift keyword.
        let protocolName: String
        let resize: Bool
        let clipboard: Bool
        /// Whether this session *can* carry sound — an RDP target configured for it —
        /// not whether any is playing, and not whether the remote's audio channel is
        /// even up. From this end those are indistinguishable (docs/remote-audio.md).
        ///
        /// Required, not defaulted, like `Resize.scale` and for the same reason: a
        /// gateway old enough to omit it is one this build refuses anyway, and
        /// defaulting it to false would turn "too old" into "this target has no sound",
        /// which sends the reader to the target's config instead of to the version.
        let audio: Bool

        private enum CodingKeys: String, CodingKey {
            case name
            case protocolName = "protocol"
            case resize
            case clipboard
            case audio
        }
    }

    /// One of the remote's displays, as the Display menu lists it. The strings
    /// are built by the remote and shown verbatim — the Mac knows how its own
    /// displays are named, and saying it once keeps this menu and the browser's
    /// panel reading the same.
    struct DisplayInfo: Sendable, Equatable, Decodable, Identifiable {
        /// Opaque here — whatever goes back in a `selectDisplay`.
        let id: UInt32
        /// Short enough for a menu item: "Display 2", or "Virtual display".
        let label: String
        /// The line beside it: "1600×1000 at 2x".
        let detail: String
        let main: Bool
        /// A display the remote made for this purpose rather than one of its own
        /// screens. Named around the Swift keyword.
        let isVirtual: Bool

        private enum CodingKeys: String, CodingKey {
            case id
            case label
            case detail
            case main
            case isVirtual = "virtual"
        }
    }

    /// The `audioFormat` payload: everything needed to build a decoder.
    ///
    /// `sampleRate` is 48 000 whatever the remote negotiated — libopus encodes at that
    /// rate and nothing else, so the gateway resamples on the way in
    /// (`src/opus_stream.rs`) and this is the rate the packets are actually in.
    struct AudioFormat: Sendable, Equatable, Decodable {
        /// `"opus"`, and there is no second codec to branch on: the gateway sends Opus
        /// with no fallback representation in either direction. Carried rather than
        /// assumed so a gateway that grew one is *refused* by `OpusDecoder` instead of
        /// being fed to a decoder built for something else.
        let codec: String
        let sampleRate: Double
        let channels: UInt32
        /// `OpusHead` (RFC 7845 §5.1), verbatim.
        ///
        /// base64 on the wire because a text frame cannot carry bytes — the same reason
        /// the cursor's PNG is — and 19 bytes once a session is not worth a second
        /// binary frame kind. macOS takes none of it as a magic cookie (measured: the
        /// system decoder ignores one), so what this is *for* here is the pre-skip
        /// inside it; see `OpusDecoder`.
        let head: Data

        private enum CodingKeys: String, CodingKey {
            case codec
            case sampleRate
            case channels
            case head
        }

        /// Hand-written for `head` alone: base64 that will not decode is drift worth
        /// reporting as `malformed`, right here where the tag is still known, rather
        /// than an empty `Data` that fails later inside a decoder as "this browser
        /// cannot play Opus".
        init(from decoder: any Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            codec = try container.decode(String.self, forKey: .codec)
            sampleRate = try container.decode(Double.self, forKey: .sampleRate)
            channels = try container.decode(UInt32.self, forKey: .channels)
            let encoded = try container.decode(String.self, forKey: .head)
            guard let bytes = Data(base64Encoded: encoded) else {
                throw DecodingError.dataCorruptedError(
                    forKey: .head,
                    in: container,
                    debugDescription: "head is not base64"
                )
            }
            head = bytes
        }
    }

    struct Clipboard: Sendable, Equatable, Decodable {
        let text: String
        /// When remotex observed the change. Nil is honest for content that
        /// predates the session — VNC and RDP expose no clipboard timestamp.
        let changedAtMs: Int64?
        /// True for the answer to a `clipboardRequest`, false for an unsolicited
        /// push. Load-bearing: only a push may write the local pasteboard.
        let requested: Bool
        /// The remote's clipboard size when it was refused for exceeding
        /// `MAX_CLIPBOARD_BYTES`; `text` is empty then. This is what keeps
        /// "too large" apart from "the remote has copied nothing".
        let oversizedBytes: Int64?
    }

    /// Every tag this build understands, for the wire-contract test.
    static let allTags: Set<String> = [
        "resize", "cursor", "error", "picker", "connected", "remoteOs",
        "clipboard", "displays", "audioFormat",
    ]

    /// Tags this build knows exist and deliberately does not implement.
    ///
    /// Empty, and kept for the same reason as `ClientMessage.unsentTags`: an entry here
    /// is a decision written down, and none is outstanding. One would decode to
    /// `.unsupported` and be stepped over.
    static let ignoredTags: Set<String> = []
}

/// Why a text frame could not be turned into a `ServerMessage`.
///
/// The two cases are deliberately apart. An unknown `type` is not an error at
/// all (it becomes `.unsupported`); `untagged` means the frame was not a tagged
/// JSON object, and `malformed` means a tag this build *does* know arrived with
/// a payload it could not read — which is real protocol drift worth logging,
/// and is handled the way `src/ws.rs` handles a bad client message: drop the one
/// frame, keep the session.
enum ServerMessageError: Error, Equatable {
    case untagged
    case malformed(type: String, detail: String)
}

extension ServerMessage: Decodable {
    private enum TagKey: String, CodingKey {
        case type
    }

    private struct Resize: Decodable {
        let w: UInt16
        let h: UInt16
        /// Required, not defaulted: the gateway has sent it since
        /// `PROTOCOL_VERSION` 2, and this build refuses a gateway that speaks
        /// anything else — so a `resize` without one is drift worth reporting
        /// rather than guessing 1x through.
        let scale: Double
    }

    private struct Message: Decodable {
        let message: String
    }

    private struct RemoteOs: Decodable {
        let macos: Bool
    }

    private struct Displays: Decodable {
        let active: UInt32
        let displays: [DisplayInfo]
    }

    init(from decoder: any Decoder) throws {
        guard let tagged = try? decoder.container(keyedBy: TagKey.self),
              let type = try? tagged.decode(String.self, forKey: .type)
        else {
            throw ServerMessageError.untagged
        }
        // Each payload struct decodes from the same decoder, ignoring `type`.
        do {
            switch type {
            case "resize":
                let payload = try Resize(from: decoder)
                self = .resize(w: payload.w, h: payload.h, scale: payload.scale)
            case "cursor":
                self = .cursor(try Cursor(from: decoder))
            case "error":
                self = .error(message: try Message(from: decoder).message)
            case "picker":
                self = .picker
            case "connected":
                self = .connected(try Connected(from: decoder))
            case "remoteOs":
                self = .remoteOs(macos: try RemoteOs(from: decoder).macos)
            case "clipboard":
                self = .clipboard(try Clipboard(from: decoder))
            case "displays":
                let payload = try Displays(from: decoder)
                self = .displays(active: payload.active, displays: payload.displays)
            case "audioFormat":
                self = .audioFormat(try AudioFormat(from: decoder))
            default:
                self = .unsupported(type: type)
            }
        } catch let error as DecodingError {
            throw ServerMessageError.malformed(type: type, detail: String(describing: error))
        }
    }

    private struct Tag: Decodable {
        let type: String
    }

    /// Decode one text frame off the socket.
    ///
    /// Two passes, because the tag has to be known before the payload can be
    /// blamed for anything: a value out of its Swift type's range (a `w` past
    /// 65535, say) is reported by `JSONDecoder` as a corrupt *document* rather
    /// than as a bad key, so decoding straight into `Self` cannot tell that
    /// apart from a frame that was never JSON. Reading the tag alone first
    /// touches no payload field and so cannot hit that.
    static func decode(_ text: String) throws -> ServerMessage {
        let data = Data(text.utf8)
        let decoder = JSONDecoder()
        guard let tag = try? decoder.decode(Tag.self, from: data) else {
            throw ServerMessageError.untagged
        }
        do {
            return try decoder.decode(ServerMessage.self, from: data)
        } catch let error as ServerMessageError {
            throw error
        } catch {
            throw ServerMessageError.malformed(
                type: tag.type,
                detail: String(describing: error)
            )
        }
    }
}
