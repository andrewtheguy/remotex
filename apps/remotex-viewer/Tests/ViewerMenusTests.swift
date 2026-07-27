import AppKit
import Testing
@testable import RemotexViewer

@MainActor
struct ViewerMenusTests {
    /// The bug this menu exists for: with the standard menus gone there was
    /// nothing left to send `paste:`, so Command-V beeped in the login fields,
    /// the gateway address, and the clipboard draft alike.
    @Test
    func theEditMenuCarriesTheEditingChords() {
        let menu = ViewerMenus.makeEditMenu()
        let chords = Dictionary(
            uniqueKeysWithValues: menu.items
                .filter { !$0.keyEquivalent.isEmpty }
                .map { ($0.title, $0.keyEquivalent) }
        )
        #expect(chords["Cut"] == "x")
        #expect(chords["Copy"] == "c")
        #expect(chords["Paste"] == "v")
        #expect(chords["Select All"] == "a")
        #expect(chords["Undo"] == "z")
        #expect(chords["Redo"] == "z")
        let redo = try? #require(menu.items.first { $0.title == "Redo" })
        #expect(redo?.keyEquivalentModifierMask == [.command, .shift])
        #expect(
            menu.items.first { $0.title == "Paste" }?.action
                == #selector(NSText.paste(_:))
        )
    }

    /// The rest of the bar keeps none of its chords, at any depth — AppKit's own
    /// Control-Command-F on Enter Full Screen arrives long after launch, and a
    /// focused desktop would capture it and hand it to the guest.
    @Test
    func everythingOutsideTheEditMenuLosesItsChords() {
        let mainMenu = NSMenu()

        let editItem = NSMenuItem()
        let editMenu = ViewerMenus.makeEditMenu()
        editItem.submenu = editMenu
        mainMenu.addItem(editItem)

        let viewItem = NSMenuItem()
        let viewMenu = NSMenu(title: "View")
        let fullScreen = viewMenu.addItem(
            withTitle: "Enter Full Screen",
            action: nil,
            keyEquivalent: "f"
        )
        fullScreen.keyEquivalentModifierMask = [.control, .command]
        let nested = NSMenu(title: "Nested")
        nested.addItem(withTitle: "Deep", action: nil, keyEquivalent: "d")
        viewMenu.addItem(withTitle: "More", action: nil, keyEquivalent: "")
            .submenu = nested
        viewItem.submenu = viewMenu
        mainMenu.addItem(viewItem)

        ViewerMenus.stripKeyEquivalents(from: mainMenu, except: editMenu)

        #expect(fullScreen.keyEquivalent.isEmpty)
        #expect(fullScreen.keyEquivalentModifierMask == [])
        #expect(nested.items[0].keyEquivalent.isEmpty)
        #expect(editMenu.items.contains { $0.keyEquivalent == "v" })
    }

    /// Run from a change notification, and clearing an equivalent posts one.
    @Test
    func theSweepIsIdempotent() {
        let mainMenu = NSMenu()
        let item = mainMenu.addItem(withTitle: "Quit", action: nil, keyEquivalent: "q")
        let editMenu = ViewerMenus.makeEditMenu()

        ViewerMenus.stripKeyEquivalents(from: mainMenu, except: editMenu)
        ViewerMenus.stripKeyEquivalents(from: mainMenu, except: editMenu)

        #expect(item.keyEquivalent.isEmpty)
    }

    /// The exemption is by identity: another menu that happens to be called Edit
    /// is not this one.
    @Test
    func aLookalikeEditMenuIsNotExempt() {
        let mainMenu = NSMenu()
        let impostor = NSMenu(title: "Edit")
        let paste = impostor.addItem(
            withTitle: "Paste",
            action: #selector(NSText.paste(_:)),
            keyEquivalent: "v"
        )
        mainMenu.addItem(withTitle: "Edit", action: nil, keyEquivalent: "").submenu = impostor

        ViewerMenus.stripKeyEquivalents(from: mainMenu, except: ViewerMenus.makeEditMenu())

        #expect(paste.keyEquivalent.isEmpty)
    }
}
