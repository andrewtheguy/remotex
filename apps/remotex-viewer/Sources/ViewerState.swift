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

struct ViewerSessionState: Equatable {
    var screen = ViewerScreen.checking
    var connectionStatus: ViewerConnectionStatus?
    var connectedTarget: String?
    /// Whether the remote runs macOS, as the gateway's engine discovered it.
    /// Decides only whether a local Command shortcut stays Command or becomes
    /// remote Control.
    var remoteIsMac = false
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
