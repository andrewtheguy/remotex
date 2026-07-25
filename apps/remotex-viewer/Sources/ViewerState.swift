import Foundation

enum ViewerScreen: String {
    case checking
    case login
    case picker
    case desktop
}

enum ViewerConnectionStatus: String {
    case connecting
    case connected
    case reconnecting
    case busy
    case takenOver
}

/// One entry of the remote's resolution menu, in device pixels.
struct DisplayMode: Equatable, Identifiable, Hashable {
    var w: Int
    var h: Int

    var id: String { "\(w)x\(h)" }
    var label: String { "\(w) × \(h)" }
}

struct ViewerSessionState: Equatable {
    var screen = ViewerScreen.checking
    var connectionStatus: ViewerConnectionStatus?
    var connectedTarget: String?
    /// Whether the remote runs macOS, as the gateway's engine discovered it.
    /// Decides only whether a local Command shortcut stays Command or becomes
    /// remote Control.
    var remoteIsMac = false
    /// The resolutions the remote offers. Empty for every target without a
    /// menu — only a Mac agent on a virtual display fills this in.
    var displayModes: [DisplayMode] = []
    /// The remote's current size, so the menu can mark the entry in use.
    var remoteSize: DisplayMode?
    var canResize = false
    var canClipboard = false
    var canCaptureKeyboard = false
}

enum BridgeStatus: Equatable {
    case loading
    case ready
    case incompatible(String)
    case failed(String)
}
