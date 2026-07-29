import AppKit

/// The menu bar's two standing rules, in one place: no item carries a key
/// equivalent, and the Edit menu is the single exemption.
@MainActor
enum ViewerMenus {
    /// Restore standard text-editing actions. AppKit text controls depend on Edit
    /// menu key equivalents; remote keyboard capture intercepts them first.
    static func makeEditMenu() -> NSMenu {
        let menu = NSMenu(title: "Edit")
        // `undo:` and `redo:` are declared nowhere public — AppKit's own Edit menu
        // sends them by name too, and whichever `NSUndoManager` is on the
        // responder chain answers.
        menu.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        let redo = menu.addItem(
            withTitle: "Redo",
            action: Selector(("redo:")),
            keyEquivalent: "z"
        )
        redo.keyEquivalentModifierMask = [.command, .shift]
        menu.addItem(.separator())
        menu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        menu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        menu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        menu.addItem(.separator())
        menu.addItem(
            withTitle: "Select All",
            action: #selector(NSText.selectAll(_:)),
            keyEquivalent: "a"
        )
        return menu
    }

    /// Ensure the identity-tracked Edit menu survives SwiftUI menu-bar rebuilds.
    static func ensureEditMenu(in mainMenu: NSMenu, current: NSMenu?) -> NSMenu {
        // After the application menu, where a Mac app's Edit menu goes.
        let wanted = min(1, mainMenu.items.count)
        if let current, let at = mainMenu.items.firstIndex(where: { $0.submenu === current }) {
            // In the bar, but not necessarily still in the right place: AppKit
            // builds its View menu long after this one goes in and inserts it
            // *ahead* of it, which left the bar reading View, Edit. Moving it back
            // is idempotent — the next run finds it where it belongs and stops.
            if at != wanted {
                let item = mainMenu.items[at]
                mainMenu.removeItem(at: at)
                mainMenu.insertItem(item, at: wanted)
            }
            return current
        }
        let menu = makeEditMenu()
        let item = NSMenuItem()
        item.title = menu.title
        item.submenu = menu
        mainMenu.insertItem(item, at: wanted)
        return menu
    }

    /// Insert resize actions above AppKit's full-screen item. Locate it by action,
    /// not localized menu title, and remain idempotent under menu notifications.
    static func ensureResizeItems(in mainMenu: NSMenu, target: AnyObject) {
        for item in mainMenu.items {
            guard
                let menu = item.submenu,
                let fullScreen = menu.items.firstIndex(where: {
                    $0.action == #selector(NSWindow.toggleFullScreen(_:))
                })
            else {
                continue
            }
            // Already ours: the menu SwiftUI rebuilt is not this one, so identity
            // is not available — but these actions are, and nothing else sends
            // them.
            if menu.items.contains(where: { $0.action == resizeToWindowAction }) {
                continue
            }
            // Above the item, in the order the two are reached for, with a rule
            // between. Inserted rather than appended so this reads the same
            // whether or not AppKit has put anything else in here.
            var at = fullScreen
            for (title, action) in [
                ("Resize to Window", resizeToWindowAction),
                ("Resize to Display", resizeToDisplayAction),
            ] {
                let resize = NSMenuItem(title: title, action: action, keyEquivalent: "")
                // Weak, like every menu item's target: the model outlives the bar.
                resize.target = target
                menu.insertItem(resize, at: at)
                at += 1
            }
            menu.insertItem(.separator(), at: at)
        }
    }

    /// The actions the items above send, which is also how they are recognised.
    ///
    /// `#selector` rather than a string, and the difference is not only the
    /// compiler warning it silences: renaming either method on `ResizeMenuTarget`
    /// is now a build error instead of two menu items that still appear, still
    /// validate, and quietly do nothing when clicked. The `@objc(…)` names there
    /// pin the selector, so this stays the same string it always was.
    static let resizeToWindowAction = #selector(
        ResizeMenuTarget.resizeToWindowFromMenu(_:)
    )
    static let resizeToDisplayAction = #selector(
        ResizeMenuTarget.resizeToDisplayFromMenu(_:)
    )

    /// Clear key equivalents except beneath the identity-tracked Edit menu.
    static func stripKeyEquivalents(from menu: NSMenu?, except exempt: NSMenu?) {
        guard let menu else {
            return
        }
        for item in menu.items {
            if let submenu = item.submenu, submenu === exempt {
                continue
            }
            if !item.keyEquivalent.isEmpty {
                item.keyEquivalent = ""
                item.keyEquivalentModifierMask = []
            }
            stripKeyEquivalents(from: item.submenu, except: exempt)
        }
    }
}
