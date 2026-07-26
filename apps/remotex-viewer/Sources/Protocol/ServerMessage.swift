import Foundation

/// Gateway -> viewer control messages: `ControlMsg` in `src/protocol.rs`, which
/// is where the exact JSON is pinned by tests worth reading alongside this.
/// Screen tiles are binary frames and decode through `TileFrame` instead.
enum ServerMessage: Sendable, Equatable {
    case resize(w: UInt16, h: UInt16)
    case cursor(Cursor)
    /// A fatal engine error. Not a dead end: the session returns to the picker,
    /// so this is shown there.
    case error(message: String)
    case picker
    case connected(Connected)
    case remoteOs(macos: Bool)
    case clipboard(Clipboard)
    /// Replaced wholesale, never merged — the Mac regenerates the list on every
    /// display reconfigure, so merging keeps sizes that no longer exist. An
    /// empty list means there is no menu to offer.
    case displayModes(modes: [DisplayMode])
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

        private enum CodingKeys: String, CodingKey {
            case name
            case protocolName = "protocol"
            case resize
            case clipboard
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
        "clipboard", "displayModes",
    ]
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
    }

    private struct Message: Decodable {
        let message: String
    }

    private struct RemoteOs: Decodable {
        let macos: Bool
    }

    private struct DisplayModes: Decodable {
        let modes: [DisplayMode]
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
                self = .resize(w: payload.w, h: payload.h)
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
            case "displayModes":
                self = .displayModes(modes: try DisplayModes(from: decoder).modes)
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
