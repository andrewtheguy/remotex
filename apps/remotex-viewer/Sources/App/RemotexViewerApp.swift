import AppKit
import Foundation
import SwiftUI

@MainActor
private final class ViewerApplicationDelegate: NSObject, NSApplicationDelegate {
    /// The top-level menus this app declares in `RemoteCommands`, beside the app
    /// menu and the Edit menu inserted below.
    ///
    /// Matched by title, unlike the Edit menu's identity check, because these are
    /// SwiftUI's to build and it hands back a new `NSMenu` on every rebuild —
    /// there is no object to keep. Anything else in the bar is AppKit's own and
    /// is removed. A `CommandMenu` added to `RemoteCommands` without its title
    /// here is stripped on the next rebuild, which looks like a menu that
    /// flickers and then vanishes.
    private static let ownMenuTitles: Set<String> = ["Remote", "Display"]

    /// The one menu whose chords are kept — see `ViewerMenus.makeEditMenu`. Held
    /// so the sweep below can recognise it by identity, and so a rebuilt bar can
    /// be told apart from one that still has ours in it.
    private var editMenu: NSMenu?

    /// What the View menu's resize items send to.
    ///
    /// Held here because an `NSMenuItem`'s target is weak: nothing else in the bar
    /// would keep it alive. Set from `RemotexViewerApp` rather than built here —
    /// `NSApplicationDelegateAdaptor` creates this delegate, and there is no model
    /// to bind to until the scene does.
    private var resizeMenuTarget: ResizeMenuTarget?

    /// Give the delegate the model its View menu items act on.
    ///
    /// Called from the scene, because that is where the model is. Enforces the bar
    /// once afterwards rather than waiting: AppKit's full-screen item usually
    /// arrives later and its arrival would run this anyway, but on a launch where
    /// it is already in the bar nothing else would post a notification and the
    /// items would be missing until something unrelated changed.
    func bind(model: AppModel) {
        guard resizeMenuTarget == nil else {
            return
        }
        resizeMenuTarget = ResizeMenuTarget(model: model)
        enforceMenuBarRules()
    }

    /// Set while `enforceMenuBarRules` is running, in case a notification is ever
    /// delivered inside a menu mutation rather than after one: re-entering the
    /// check mid-insert would find the menu not yet in the bar and insert a second.
    private var isEnforcingMenuBarRules = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Both menu bar rules are enforced as items arrive rather than once over what
        // is there at launch, because the bar this app hands out is not the last one
        // it gets. AppKit builds the View menu holding Enter Full Screen only when a
        // window can go full screen, which is later than this, and it arrives with
        // Control-Command-F on it: that chord is delivered to this app like any other,
        // so a focused desktop captures it and the guest gets it instead — the item
        // would name a shortcut that does something else entirely. SwiftUI, for its
        // part, rebuilds the whole bar from its own model when the first window comes
        // up, which drops the Edit menu inserted below. Each rebuild is another
        // arrival, and both rules are re-applied over it.
        // `RemoteCommands` says the rest of why; this is the half of it that AppKit's
        // own items can only be held to here.
        //
        // A *removal* is one of the three, and the one that matters most for the Edit
        // menu being there at all. A rebuild that drops our item and adds nothing
        // after it posts only this notification: without it the menu came back on the
        // next unrelated change to the bar — a clipboard fetch starting, a display
        // list arriving — which is a menu that appears while you are looking at it,
        // and a bar whose entries move under the pointer. It goes back the moment it
        // goes.
        for name in [
            NSMenu.didAddItemNotification,
            NSMenu.didChangeItemNotification,
            NSMenu.didRemoveItemNotification,
        ] {
            NotificationCenter.default.addObserver(
                forName: name,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated {
                    self?.enforceMenuBarRules()
                }
            }
        }
        DispatchQueue.main.async { [weak self] in
            guard let self, let mainMenu = NSApp.mainMenu else {
                return
            }

            // SwiftUI retains standard menus that command-group replacements cannot remove.
            for item in mainMenu.items.dropFirst()
            where !Self.ownMenuTitles.contains(item.title) {
                mainMenu.removeItem(item)
            }
            self.enforceMenuBarRules()
        }
    }

    /// The Edit menu is in the bar, and nothing outside it carries a chord.
    ///
    /// Idempotent, which is what makes it safe to run from a change notification —
    /// the work it does posts more of them. `ViewerMenus.ensureEditMenu` says why
    /// the menu has to be put back rather than installed once.
    private func enforceMenuBarRules() {
        guard !isEnforcingMenuBarRules, let mainMenu = NSApp.mainMenu else {
            return
        }
        isEnforcingMenuBarRules = true
        defer { isEnforcingMenuBarRules = false }
        editMenu = ViewerMenus.ensureEditMenu(in: mainMenu, current: editMenu)
        if let resizeMenuTarget {
            ViewerMenus.ensureResizeItems(in: mainMenu, target: resizeMenuTarget)
        }
        // From the menu bar down rather than from the menu that posted: `Notification`
        // cannot cross into the actor, and starting at the root also keeps a context
        // menu — AppKit's to spell, and whose chords the text field handles rather
        // than the item — out of reach. The bar is a dozen items; walking it is free.
        ViewerMenus.stripKeyEquivalents(from: mainMenu, except: editMenu)
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
    // Both from `ViewerDefaults`, so a `--settings` run keeps the real gateway
    // address and the real login untouched. See that file.
    @State private var model = AppModel(
        defaults: ViewerDefaults.resolved,
        urlSession: ViewerDefaults.urlSession
    )

    var body: some Scene {
        WindowGroup {
            RootView(model: model)
                .frame(minWidth: 900, minHeight: 640)
                // The View menu's resize items are AppKit's and need a target —
                // see `ViewerMenus.ensureResizeItems`. This is the first point
                // where both the delegate and the model exist.
                .task { applicationDelegate.bind(model: model) }
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
