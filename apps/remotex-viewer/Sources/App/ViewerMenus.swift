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
