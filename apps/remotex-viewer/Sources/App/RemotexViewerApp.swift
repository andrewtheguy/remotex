import AppKit
import Foundation
import SwiftUI

@MainActor
private final class ViewerApplicationDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // No item in this menu bar carries a key equivalent, and the rule is enforced
        // as items arrive rather than once over what is there at launch: AppKit builds
        // the View menu holding Enter Full Screen only when a window can go full
        // screen, which is later than this, and it arrives with Control-Command-F on
        // it. That chord is delivered to this app like any other, so a focused desktop
        // captures it and the guest gets it instead — the item would name a shortcut
        // that does something else entirely. `RemoteCommands` says the rest of why;
        // this is the half of it that AppKit's own items can only be held to here.
        for name in [NSMenu.didAddItemNotification, NSMenu.didChangeItemNotification] {
            NotificationCenter.default.addObserver(
                forName: name,
                object: nil,
                queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    // From the menu bar down rather than from the menu that posted:
                    // `Notification` cannot cross into the actor, and starting at the
                    // root also keeps a context menu — AppKit's to spell, and whose
                    // chords the text field handles rather than the item — out of
                    // reach. The bar is a dozen items; walking it is free.
                    Self.stripKeyEquivalents(from: NSApp.mainMenu)
                }
            }
        }
        DispatchQueue.main.async {
            guard let mainMenu = NSApp.mainMenu else {
                return
            }

            // SwiftUI retains standard menus that command-group replacements cannot remove.
            for item in mainMenu.items.dropFirst() where item.title != "Remote" {
                mainMenu.removeItem(item)
            }
            Self.stripKeyEquivalents(from: mainMenu)
        }
    }

    /// Idempotent, which is what keeps it safe to run from a change notification:
    /// clearing an equivalent posts one of those in turn, and an item that has none
    /// already is left alone. A modifier mask with no character is inert.
    private static func stripKeyEquivalents(from menu: NSMenu?) {
        guard let menu else {
            return
        }
        for item in menu.items {
            if !item.keyEquivalent.isEmpty {
                item.keyEquivalent = ""
                item.keyEquivalentModifierMask = []
            }
            stripKeyEquivalents(from: item.submenu)
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
