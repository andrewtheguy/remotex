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

    /// Refuse the first quit, and mean the second one.
    ///
    /// Not a veto: the engine cannot be taken down from here, because a ⌘Q is
    /// delivered from *inside* one of Chromium's own pump slices and nothing asked of
    /// CEF on that stack is done. `ChromiumHost.stopThenQuit` steps off it, closes
    /// the browsers, shuts the engine down and calls `terminate` again — which
    /// arrives here with the engine already gone and is allowed straight through.
    ///
    /// `terminateLater` looks like the mechanism for this and is not: AppKit waits
    /// for the reply on the same stack, so the slice being waited for is the one the
    /// waiting is inside. That was measured, twice.
    ///
    /// What it costs to get wrong is a Chromium profile that is never shut down
    /// cleanly, which the *following* launch reports as a crash — the fault showing
    /// itself one launch after the one that caused it.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard ChromiumHost.isRunning else {
            return .terminateNow
        }
        ChromiumHost.stopThenQuit { sender.terminate(nil) }
        return .terminateCancel
    }

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
        // Nothing here sets the Dock icon. An instance that wants its own face is its
        // own bundle now (`make-instance-bundle.sh`), which carries an `AppIcon.icns`
        // like every other Mac app — so Finder, the Dock and ⌘-Tab agree about it
        // before the process has even started, which a runtime override never could.

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
/// `--version` has to be answered before an application exists: it does not want a
/// window, and doing this in `RemotexViewerApp.init()` meant SwiftUI had already
/// begun bringing one up around it.
@main
@MainActor
enum ViewerMain {
    static func main() {
        // Refuse unknown options before they can redirect an intended isolated QA
        // launch to the real instance.
        ArgumentCheck.enforce()
        if CommandLine.arguments.contains("--version") {
            print("remotex-viewer \(ProductInfo.version)")
            Foundation.exit(EXIT_SUCCESS)
        }

        // Chromium, before the window that shows it — and `NSApp` before Chromium,
        // because CEF sends `isHandlingSendEvent` to whatever application object it
        // finds and there is no second chance to replace it.
        //
        // Sent to `ViewerApplication` rather than to `NSApplication`, and that is the
        // whole of it: `shared` is `+[NSApplication sharedApplication]`, which builds
        // an instance of the class it was sent to. `NSPrincipalClass` is read by
        // `NSApplicationMain` alone, which SwiftUI reaches long after this — so the
        // plist entry never had a chance to matter and `NSApplication.shared` here
        // quietly made a plain one. What that looked like was not a bad application
        // object: it was an uncaught `unrecognized selector` the moment Chromium
        // handled an event, i.e. a window that came up and died at the first click.
        _ = ViewerApplication.shared

        // An unbundled build has no SPA and no engine to point at one. It is not an
        // error to skip: `swift test` and `--version` both take this path, and the
        // window's own launch screen already says what a bundle-less build cannot do.
        if let webRoot = GatewayBinary.webRootInBundle() {
            let instance = InstanceDirectory.resolved()
            // Before the engine, because the engine's profile is *inside* this
            // directory and creating a child creates the parent as a side effect —
            // with the process umask rather than the `0700` this one has to have.
            // `remotex.toml` beside it holds the credentials of every machine this
            // app can reach, and `create` does not tighten a directory that already
            // exists, so the order here is the only thing that sets the mode.
            try? instance.create()
            guard ChromiumHost.start(webRoot: webRoot, profile: instance.browserProfile) else {
                // Terminal. There is no second engine to fall back to, and every
                // screen this app has is drawn by the page that engine would show —
                // so continuing would open a window that can never paint anything.
                FileHandle.standardError.write(Data(
                    """
                    remotex-viewer: Chromium refused to start; \
                    nothing can be displayed. Run \
                    \(Bundle.main.executablePath ?? "remotex-viewer") \
                    with REMOTEX_CEF_TRACE=1 to see how far it got.

                    """.utf8
                ))
                Foundation.exit(EXIT_FAILURE)
            }
        }

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
                // Nothing is contacted from here. The app opens on the home screen,
                // which is the one question it cannot answer for anybody: the gateway
                // in this bundle is the right one for targets this Mac can reach
                // directly, and the wrong one when the link to them is slow enough
                // that the gateway belongs at the other end. Starting the local one
                // unasked is what left no way to say so.
                .task {
                    applicationDelegate.bind(model: model)
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
