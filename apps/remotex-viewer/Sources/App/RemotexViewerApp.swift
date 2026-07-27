import AppKit
import Foundation
import SwiftUI

@MainActor
private final class ViewerApplicationDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        DispatchQueue.main.async {
            guard let mainMenu = NSApp.mainMenu else {
                return
            }

            // SwiftUI retains standard menus that command-group replacements cannot remove.
            for item in mainMenu.items.dropFirst() where item.title != "Remote" {
                mainMenu.removeItem(item)
            }
        }
    }
}

/// The process entry point, ahead of SwiftUI.
///
/// Both command-line paths have to be answered before an application exists:
/// `--probe` runs its own main loop and never returns, and neither it nor
/// `--version` wants a window. Doing this in `RemotexViewerApp.init()` meant
/// SwiftUI had already begun bringing one up around them.
@main
@MainActor
enum ViewerMain {
    static func main() {
        if CommandLine.arguments.contains("--version") {
            print("remotex-viewer \(ProductInfo.version)")
            Foundation.exit(EXIT_SUCCESS)
        }
        ProbeCommand.runIfRequested()

        NSWindow.allowsAutomaticWindowTabbing = false
        RemotexViewerApp.main()
    }
}

struct RemotexViewerApp: App {
    @NSApplicationDelegateAdaptor(ViewerApplicationDelegate.self)
    private var applicationDelegate
    @State private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            RootView(model: model)
                .frame(minWidth: 900, minHeight: 640)
        }
        .defaultSize(width: 1440, height: 900)
        // The compact bar, because the toolbar's height is the desktop's loss: the
        // title bar arrives as a top `contentInset` on the scroll view, so every
        // point of it is a point the remote is not shown in (and, for a guest that
        // follows the window, not *given*). The default unified style is 52pt tall
        // for one button; compact measures 40 and reads the same — a VNC guest
        // that follows the window came back 12 rows taller for it.
        .windowToolbarStyle(.unifiedCompact)
        .commandsReplaced {
            RemoteCommands(model: model)
        }
        // No Settings scene: the gateway address lives on the login screen, next
        // to the credentials it goes with.
    }
}
