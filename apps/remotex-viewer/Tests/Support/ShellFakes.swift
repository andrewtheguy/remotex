import AppKit
import Foundation
@testable import RemotexViewer

/// A page that records what it was told instead of running it.
///
/// Every menu item ends in exactly one `NativeCommand`, so this is the whole
/// observable output of the menu bar. The commands are compared as values — the
/// JSON they turn into is `NativeCommandTests`' business, once, rather than
/// re-asserted by everything that sends one.
@MainActor
final class RecordingSink: CommandSink {
    private(set) var commands: [NativeCommand] = []

    func send(_ command: NativeCommand) {
        commands.append(command)
    }

    /// Whether this command was sent, ignoring anything else that was.
    func sent(_ command: NativeCommand) -> Bool {
        commands.contains(command)
    }

    func clear() {
        commands.removeAll()
    }
}

@MainActor
extension AppModel {
    /// A model with a throwaway pasteboard, no gateway, and a sink to read the
    /// menus' output off.
    ///
    /// `nil` for the gateway is the unbundled case this suite runs in: there is no
    /// `remotex-gateway` beside a test binary, and nothing here needs one — the page
    /// is what the menus talk to and the page is the sink.
    static func underTest(sink: RecordingSink) -> AppModel {
        let model = withoutPage()
        model.showPage(sink: sink)
        return model
    }

    /// The same model before there is a page in the window — the launch screen,
    /// where a menu has nothing to talk to.
    static func withoutPage() -> AppModel {
        AppModel(
            clipboard: ClipboardSynchronizer(
                pasteboard: NSPasteboard.withUniqueName(),
                startsPolling: false
            )
        )
    }

    /// Put the model on the screen a page occupies, with that page attached.
    ///
    /// `launch()` cannot get here without a gateway to start, and starting one is
    /// not what any of these tests are about.
    func showPage(sink: RecordingSink) {
        showReadyForTesting(GatewayEndpoint(port: 49_213, token: "test-token"))
        attach(bridge: sink)
    }

    /// The page's report, built for a test.
    ///
    /// Assembled through `NativeState` rather than through JSON so a field that is
    /// renamed on one side of the bridge fails to compile here; that the JSON
    /// decodes into these fields is `NativeStateTests`' job.
    static func desktopState(
        status: NativeState.Status = .connected,
        size: NativeState.RemoteSize? = NativeState.RemoteSize(w: 1920, h: 1080, scale: 1),
        canResize: Bool = false,
        canAutoResize: Bool = false,
        autoResize: Bool = false,
        canClipboard: Bool = false,
        canAudio: Bool = false,
        audioEnabled: Bool = false,
        displays: [DisplayChoice] = [],
        activeDisplayId: UInt32? = nil,
        macKeyOverridesEnabled: Bool = true,
        macKeyOverridesActive: Bool = true,
        remoteIsMac: Bool = false
    ) -> NativeState {
        var state = NativeState()
        state.mode = .desktop
        state.status = status
        state.ready = size != nil
        state.size = size
        state.canResize = canResize
        state.canAutoResize = canAutoResize
        state.autoResize = autoResize
        state.canClipboard = canClipboard
        state.canAudio = canAudio
        state.audioEnabled = audioEnabled
        state.displays = displays
        state.activeDisplayId = activeDisplayId
        state.macKeyOverridesEnabled = macKeyOverridesEnabled
        state.macKeyOverridesActive = macKeyOverridesActive
        state.remoteIsMac = remoteIsMac
        return state
    }
}
