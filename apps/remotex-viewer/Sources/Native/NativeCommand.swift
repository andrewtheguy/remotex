import Foundation

/// Whatever a command can be sent to — the page, in the app; a recorder, in a test.
///
/// The seam exists because every menu item ends in one of these calls and nothing
/// else, so a test that can see the commands can see the whole menu bar. It is not
/// an abstraction over the page: there is one implementation and it is
/// `NativeBridge`.
@MainActor
protocol CommandSink: AnyObject {
    func send(_ command: NativeCommand)

    /// Open the engine's inspector on the page.
    ///
    /// Not a `NativeCommand`: it is the one menu item the page is not told about,
    /// because the thing it acts on is the engine and not the client. Defaulted to
    /// nothing so a recorder in a test stays a list of commands.
    func showDevTools()
}

extension CommandSink {
    func showDevTools() {}
}

/// What the app tells the page: a key it was given, the Mac's pasteboard, or a
/// menu item standing in for a control the shell hides.
///
/// The mirror of `NativeCommand` in `frontend/src/nativeHost.ts`. Encoded rather
/// than assembled as JavaScript so a clipboard value can hold anything at all — a
/// string interpolated into a `evaluateJavaScript` call is code, and text copied
/// off a remote desktop is not this app's to trust.
enum NativeCommand: Equatable {
    /// One key, already mapped from a macOS virtual keycode to a DOM `code`. The
    /// page's translator turns it into what goes on the wire.
    case key(NativeKeyEvent)
    /// Let go of everything held. Sent where a browser would see a `blur`.
    case releaseInput
    /// The Mac's pasteboard changed; the page forwards it to the remote if its own
    /// echo guards agree.
    case clipboardLocal(String)
    case openClipboard
    case openDisplays
    case closePanel
    case resizeToWindow
    case setAutoResize(Bool)
    case selectDisplay(UInt32)
    case setAudio(Bool)
    case setMacKeyOverrides(Bool)
    case refresh
    case switchTarget
    case takeOver
    /// A chord as a sequence of codes, pressed in order and released in reverse —
    /// the Send Keys menu.
    case sendKeyCombo([String])

    /// The JSON body, exactly as `nativeHost.ts` discriminates it.
    var body: [String: Any] {
        switch self {
        case .key(let event):
            return [
                "type": "key",
                "code": event.code,
                "pressed": event.pressed,
                "caps": event.caps,
                "meta": event.meta,
            ]
        case .releaseInput:
            return ["type": "releaseInput"]
        case .clipboardLocal(let text):
            return ["type": "clipboardLocal", "text": text]
        case .openClipboard:
            return ["type": "openClipboard"]
        case .openDisplays:
            return ["type": "openDisplays"]
        case .closePanel:
            return ["type": "closePanel"]
        case .resizeToWindow:
            return ["type": "resizeToWindow"]
        case .setAutoResize(let enabled):
            return ["type": "setAutoResize", "enabled": enabled]
        case .selectDisplay(let id):
            return ["type": "selectDisplay", "id": id]
        case .setAudio(let enabled):
            return ["type": "setAudio", "enabled": enabled]
        case .setMacKeyOverrides(let enabled):
            return ["type": "setMacKeyOverrides", "enabled": enabled]
        case .refresh:
            return ["type": "refresh"]
        case .switchTarget:
            return ["type": "switchTarget"]
        case .takeOver:
            return ["type": "takeOver"]
        case .sendKeyCombo(let codes):
            return ["type": "sendKeyCombo", "codes": codes]
        }
    }

    /// The call to evaluate, or nil for a body that will not encode.
    ///
    /// `??.` throughout: the page installs its entry point when the desktop mounts
    /// and removes it when that unmounts, so a command sent a moment either side of
    /// a target switch finds nothing there, and finding nothing there is correct.
    func javaScript() -> String? {
        guard let data = try? JSONSerialization.data(withJSONObject: body),
              let json = String(data: data, encoding: .utf8)
        else {
            return nil
        }
        return "window.__remotexNative?.command?.(\(json))"
    }
}

/// What the page tells the app.
///
/// The mirror of `NativeEvent` in `frontend/src/nativeHost.ts`.
enum NativeEvent: Equatable {
    case state(NativeState)
    /// The remote's clipboard changed on its own, and the page cannot write
    /// `NSPasteboard`.
    case clipboardFromRemote(String)
    /// The gateway did not accept the launch token. Nothing the page can show
    /// helps, so the app takes the screen back.
    case unauthenticated

    /// Decode one `postMessage` body.
    ///
    /// The body arrives as Foundation collections, so it is re-encoded and run
    /// through `JSONDecoder` rather than picked apart key by key — the state object
    /// has sixteen fields and a hand-written reader is sixteen chances to disagree
    /// with the page about one of them.
    static func decode(_ body: Any) -> NativeEvent? {
        guard let object = body as? [String: Any],
              let type = object["type"] as? String
        else {
            return nil
        }
        switch type {
        case "state":
            guard let state = object["state"],
                  let data = try? JSONSerialization.data(withJSONObject: state),
                  let decoded = try? JSONDecoder().decode(NativeState.self, from: data)
            else {
                return nil
            }
            return .state(decoded)
        case "clipboardFromRemote":
            guard let text = object["text"] as? String else {
                return nil
            }
            return .clipboardFromRemote(text)
        case "unauthenticated":
            return .unauthenticated
        default:
            return nil
        }
    }
}
