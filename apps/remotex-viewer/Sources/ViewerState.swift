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

enum GuestOS: String {
    case windows
    case macos
    case linux
}

struct ViewerSessionState: Equatable {
    var screen = ViewerScreen.checking
    var connectionStatus: ViewerConnectionStatus?
    var connectedTarget: String?
    var guestOS: GuestOS?
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
