import AppKit
import Foundation
import SwiftUI

@MainActor
private final class ViewerApplicationDelegate: NSObject, NSApplicationDelegate {
    /// SwiftUI-owned top-level menus, matched by title across rebuilds.
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
        gateway = model.gateway
        enforceMenuBarRules()
    }

    /// Set while `enforceMenuBarRules` is running, in case a notification is ever
    /// delivered inside a menu mutation rather than after one: re-entering the
    /// check mid-insert would find the menu not yet in the bar and insert a second.
    private var isEnforcingMenuBarRules = false

    /// The gateway to stop when this app does. Set by the scene, for the same reason
    /// `resizeMenuTarget` is: the delegate exists before there is a model.
    private var gateway: EmbeddedGateway?

    /// Stop the gateway on the way out.
    ///
    /// Belt on top of braces. The guarantee is the liveness pipe — the kernel closes
    /// our end of it however this process ends, and the gateway exits on the EOF (see
    /// `EmbeddedGateway`) — so this is only here to make the ordinary quit immediate
    /// rather than a few milliseconds later. It must stay synchronous: the process may
    /// be gone the moment this returns, so there is nothing to await in.
    func applicationWillTerminate(_ notification: Notification) {
        gateway?.terminateNow()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        // A Dock icon for this instance, if its directory carries one. Done here rather
        // than at the process entry point because `NSApp` does not exist until SwiftUI
        // has built it, and resolved from the arguments rather than passed in because
        // that is all it depends on — the model has nothing to do with how the app
        // looks. See `InstanceDirectory.iconURL`.
        if let icon = InstanceDirectory.resolved().iconURL(),
           let image = NSImage(contentsOf: icon)
        {
            NSApp.applicationIconImage = image
        }

        // SwiftUI and AppKit build and rebuild menus after launch. Reapply the
        // Edit-menu and no-shortcut rules on additions, changes, and removals.
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
    /// Everything this launch owns comes from one instance directory — the config,
    /// the log, the preferences — so `--instance-dir` swaps all of it at once. See
    /// `InstanceDirectory`.
    @State private var model = AppModel.forApp()

    var body: some Scene {
        WindowGroup {
            RootView(model: model)
                .frame(minWidth: 900, minHeight: 640)
                // The View menu's resize items are AppKit's and need a target —
                // see `ViewerMenus.ensureResizeItems`. This is the first point
                // where both the delegate and the model exist.
                .task {
                    applicationDelegate.bind(model: model)
                    // Unlike the old launch, which deliberately contacted nothing:
                    // there is no address to be wrong now, and the gateway is ours to
                    // start. Waiting for a button here would be waiting for the user
                    // to confirm the only thing this app can do.
                    await model.launch()
                }
        }
        .defaultSize(width: 1440, height: 900)
        // Toolbar height reduces the viewport reported to a following guest.
        .windowToolbarStyle(.unifiedCompact)
        .commandsReplaced {
            RemoteCommands(model: model)
        }
        // No Settings scene: what there is to configure is the gateway's own config
        // file, and it is edited in a sheet over the window (`ConfigurationPanel`) so
        // that saving it can restart the gateway the window is attached to.
    }
}
