import AppKit

/// The menu bar's two standing rules, in one place: no item carries a key
/// equivalent, and the Edit menu is the single exemption.
@MainActor
enum ViewerMenus {
    /// The standard editing chords, put back as a menu of our own.
    ///
    /// Command-C, Command-V, Command-X and Command-A are not built into
    /// `NSTextField` or `NSTextView`. On macOS they are Edit menu key
    /// equivalents, and the responder chain sees `copy:`/`paste:` at all only
    /// because a menu item sent them. Taking the standard menus down took that
    /// with it, so every text field in the app — the login credentials, the
    /// gateway address, the clipboard draft — answered Command-V with a beep.
    ///
    /// Exempting this menu does not weaken the no-key-equivalents rule, which is
    /// about a chord whose meaning depends on which screen is up. These items
    /// have one meaning wherever they fire: while the desktop is focused
    /// `KeyboardCapture` takes the chord before the menu bar is offered it, and
    /// with no text field in the responder chain to answer `paste:` the item is
    /// disabled anyway. They light up exactly where they say what they do.
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

    /// Put the Edit menu into `mainMenu` unless `current` is already in it, and
    /// return the one that is there now — to hold for the next check, and to hand
    /// the sweep as its exemption.
    ///
    /// Installing it once at launch looked like it had done nothing, and very
    /// nearly had: SwiftUI rebuilds the whole bar from its own model of it — when
    /// the first window comes up, and again whenever `commandsReplaced`
    /// re-evaluates — and an item this app inserted is not in that model. The menu
    /// was in the bar for about a second after launch and then gone, so every text
    /// field went back to beeping at Command-V.
    ///
    /// Membership is checked by object identity, for the same reason the sweep's
    /// exemption is: an item titled "Edit" in a rebuilt bar is not evidence that
    /// this menu survived it.
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

    /// Put the two resize items above AppKit's full-screen item.
    ///
    /// They live in AppKit's View menu rather than being declared in
    /// `RemoteCommands`, which says why: a `CommandMenu("View")` is dropped by
    /// SwiftUI and leaves the bar with two View menus holding nothing.
    ///
    /// Found by the full-screen item rather than by menu title, and that is the
    /// whole of how this locates itself: the item is the thing being sat above, it
    /// is AppKit's own, and its menu is whichever one AppKit put it in. A title is
    /// a localized string, and this app has already been bitten by two menus
    /// answering to "View".
    ///
    /// Does nothing until that item exists — which for a window that cannot go
    /// full screen is never. There is no View menu to add to before then, and a
    /// menu of our own is what this is avoiding.
    ///
    /// Idempotent, which is what makes it safe to run from a menu-change
    /// notification: inserting these items posts three more of those.
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
    static let resizeToWindowAction = Selector(("resizeToWindowFromMenu:"))
    static let resizeToDisplayAction = Selector(("resizeToDisplayFromMenu:"))

    /// Clear every key equivalent in `menu`, except in `exempt` and below it.
    ///
    /// Idempotent, which is what keeps it safe to run from a change notification:
    /// clearing an equivalent posts one of those in turn, and an item that has
    /// none already is left alone. A modifier mask with no character is inert.
    ///
    /// The exemption is by object identity rather than by title: the menu to keep
    /// is one this app built, and a title is a localized string that anything
    /// could also be called.
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
