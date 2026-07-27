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

    /// The bug behind the bug: the menu went in at launch and SwiftUI's first
    /// rebuild of the bar took it back out about a second later, so the fix for
    /// the beeping looked like it had done nothing at all.
    @Test
    func theEditMenuGoesBackIntoARebuiltBar() {
        let mainMenu = NSMenu()
        mainMenu.addItem(withTitle: "remotex-viewer", action: nil, keyEquivalent: "")
        mainMenu.addItem(withTitle: "Remote", action: nil, keyEquivalent: "")

        let installed = ViewerMenus.ensureEditMenu(in: mainMenu, current: nil)
        #expect(mainMenu.items.map(\.title) == ["remotex-viewer", "Edit", "Remote"])

        // Rebuilt from a model this menu is not in, the way SwiftUI does it.
        mainMenu.removeAllItems()
        mainMenu.addItem(withTitle: "remotex-viewer", action: nil, keyEquivalent: "")
        mainMenu.addItem(withTitle: "View", action: nil, keyEquivalent: "")
        mainMenu.addItem(withTitle: "Remote", action: nil, keyEquivalent: "")

        let reinstalled = ViewerMenus.ensureEditMenu(in: mainMenu, current: installed)
        #expect(reinstalled !== installed)
        #expect(mainMenu.items.map(\.title) == ["remotex-viewer", "Edit", "View", "Remote"])
        #expect(reinstalled.items.contains { $0.keyEquivalent == "v" })
    }

    /// Run from a change notification, and inserting the menu posts one.
    @Test
    func ensuringTheEditMenuTwiceLeavesOne() {
        let mainMenu = NSMenu()
        mainMenu.addItem(withTitle: "remotex-viewer", action: nil, keyEquivalent: "")

        let installed = ViewerMenus.ensureEditMenu(in: mainMenu, current: nil)
        let again = ViewerMenus.ensureEditMenu(in: mainMenu, current: installed)

        #expect(again === installed)
        #expect(mainMenu.items.filter { $0.title == "Edit" }.count == 1)
    }

    /// Membership is by identity too: a rebuilt bar carrying something else called
    /// Edit is not this menu surviving the rebuild, and the chords the sweep
    /// exempts are only on the menu this app holds.
    @Test
    func aLookalikeEditMenuDoesNotCountAsInstalled() {
        let mainMenu = NSMenu()
        mainMenu.addItem(withTitle: "remotex-viewer", action: nil, keyEquivalent: "")
        let ours = ViewerMenus.ensureEditMenu(in: mainMenu, current: nil)

        // Rebuilt without ours, but with something of its own by that name.
        mainMenu.removeAllItems()
        mainMenu.addItem(withTitle: "remotex-viewer", action: nil, keyEquivalent: "")
        let impostor = NSMenu(title: "Edit")
        mainMenu.addItem(withTitle: "Edit", action: nil, keyEquivalent: "").submenu = impostor

        let installed = ViewerMenus.ensureEditMenu(in: mainMenu, current: ours)

        #expect(installed !== impostor)
        #expect(installed !== ours)
        #expect(installed.items.contains { $0.keyEquivalent == "v" })
        #expect(mainMenu.items.filter { $0.title == "Edit" }.count == 2)
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
