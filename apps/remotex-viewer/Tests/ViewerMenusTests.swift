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

    /// AppKit's View menu is built long after the Edit menu goes in, and it is
    /// inserted ahead of it — which left the bar reading View, Edit. The Edit menu
    /// belongs immediately after the application menu wherever else things land.
    @Test
    func theEditMenuIsMovedBackAfterTheAppMenu() {
        let mainMenu = NSMenu()
        mainMenu.addItem(withTitle: "remotex-viewer", action: nil, keyEquivalent: "")
        let installed = ViewerMenus.ensureEditMenu(in: mainMenu, current: nil)
        #expect(mainMenu.items.map(\.title) == ["remotex-viewer", "Edit"])

        // AppKit puts its View menu in front of ours, as it does at launch.
        mainMenu.insertItem(
            withTitle: "View",
            action: nil,
            keyEquivalent: "",
            at: 1
        )
        #expect(mainMenu.items.map(\.title) == ["remotex-viewer", "View", "Edit"])

        let again = ViewerMenus.ensureEditMenu(in: mainMenu, current: installed)

        #expect(again === installed, "the same menu, moved rather than rebuilt")
        #expect(mainMenu.items.map(\.title) == ["remotex-viewer", "Edit", "View"])

        // Run from a change notification, and moving an item posts one.
        _ = ViewerMenus.ensureEditMenu(in: mainMenu, current: installed)
        #expect(mainMenu.items.map(\.title) == ["remotex-viewer", "Edit", "View"])
    }

    /// The three resize items go into AppKit's View menu above its full-screen
    /// item, because a `CommandMenu("View")` is dropped by SwiftUI — see
    /// `RemoteCommands`. The mode comes first, then the two one-shots it governs.
    @Test
    func theResizeItemsGoAboveFullScreen() {
        let mainMenu = NSMenu()
        let view = NSMenu(title: "View")
        view.addItem(
            withTitle: "Enter Full Screen",
            action: #selector(NSWindow.toggleFullScreen(_:)),
            keyEquivalent: ""
        )
        mainMenu.addItem(withTitle: "View", action: nil, keyEquivalent: "").submenu = view
        let target = NSObject()

        ViewerMenus.ensureResizeItems(in: mainMenu, target: target)

        #expect(
            view.items.map(\.title) == [
                "Auto Resize", "Resize to Window", "Resize to Display", "",
                "Enter Full Screen",
            ]
        )
        #expect(view.items[3].isSeparatorItem)
        #expect(view.items[0].action == ViewerMenus.autoResizeAction)
        #expect(view.items[1].action == ViewerMenus.resizeToWindowAction)
        #expect(view.items[2].action == ViewerMenus.resizeToDisplayAction)
        // A menu item's target is weak, so this is the object the delegate holds.
        #expect(view.items[0].target === target)
    }

    /// Run from a change notification, and inserting items posts more.
    @Test
    func ensuringTheResizeItemsTwiceLeavesOneSet() {
        let mainMenu = NSMenu()
        let view = NSMenu(title: "View")
        view.addItem(
            withTitle: "Enter Full Screen",
            action: #selector(NSWindow.toggleFullScreen(_:)),
            keyEquivalent: ""
        )
        mainMenu.addItem(withTitle: "View", action: nil, keyEquivalent: "").submenu = view
        let target = NSObject()

        ViewerMenus.ensureResizeItems(in: mainMenu, target: target)
        ViewerMenus.ensureResizeItems(in: mainMenu, target: target)

        #expect(view.items.filter { $0.title == "Resize to Window" }.count == 1)
        #expect(view.items.filter { $0.isSeparatorItem }.count == 1)
    }

    /// SwiftUI rebuilds the bar and hands back menus this app has never seen, so
    /// the items have to go back into the new one — recognised by their action,
    /// since there is no object left to compare.
    @Test
    func theResizeItemsGoBackIntoARebuiltBar() {
        let mainMenu = NSMenu()
        let first = NSMenu(title: "View")
        first.addItem(
            withTitle: "Enter Full Screen",
            action: #selector(NSWindow.toggleFullScreen(_:)),
            keyEquivalent: ""
        )
        mainMenu.addItem(withTitle: "View", action: nil, keyEquivalent: "").submenu = first
        let target = NSObject()
        ViewerMenus.ensureResizeItems(in: mainMenu, target: target)

        // Rebuilt: a different NSMenu, with AppKit's item back in it and ours gone.
        mainMenu.removeAllItems()
        let rebuilt = NSMenu(title: "View")
        rebuilt.addItem(
            withTitle: "Enter Full Screen",
            action: #selector(NSWindow.toggleFullScreen(_:)),
            keyEquivalent: ""
        )
        mainMenu.addItem(withTitle: "View", action: nil, keyEquivalent: "").submenu = rebuilt

        ViewerMenus.ensureResizeItems(in: mainMenu, target: target)

        #expect(
            rebuilt.items.map(\.title).prefix(3)
                == ["Auto Resize", "Resize to Window", "Resize to Display"]
        )
    }

    /// The menu is found by the full-screen item, not by its title: this app has
    /// been bitten by two menus answering to "View", and the title also flips to
    /// Exit Full Screen with the window.
    @Test
    func theItemsFollowTheFullScreenItemWhateverItsMenuIsCalled() {
        let mainMenu = NSMenu()
        let decoy = NSMenu(title: "View")
        decoy.addItem(withTitle: "Something Else", action: nil, keyEquivalent: "")
        mainMenu.addItem(withTitle: "View", action: nil, keyEquivalent: "").submenu = decoy
        let real = NSMenu(title: "Ansicht")
        real.addItem(
            withTitle: "Exit Full Screen",
            action: #selector(NSWindow.toggleFullScreen(_:)),
            keyEquivalent: ""
        )
        mainMenu.addItem(withTitle: "Ansicht", action: nil, keyEquivalent: "").submenu = real

        ViewerMenus.ensureResizeItems(in: mainMenu, target: NSObject())

        #expect(decoy.items.map(\.title) == ["Something Else"], "the decoy is left alone")
        #expect(
            real.items.map(\.title).prefix(3)
                == ["Auto Resize", "Resize to Window", "Resize to Display"]
        )
    }

    /// Until AppKit's item exists — which for a window that cannot go full screen
    /// is never — there is no menu to add to, and none is invented.
    @Test
    func aBarWithoutAFullScreenItemGainsNothing() {
        let mainMenu = NSMenu()
        let view = NSMenu(title: "View")
        view.addItem(withTitle: "Something Else", action: nil, keyEquivalent: "")
        mainMenu.addItem(withTitle: "View", action: nil, keyEquivalent: "").submenu = view

        ViewerMenus.ensureResizeItems(in: mainMenu, target: NSObject())

        #expect(view.items.map(\.title) == ["Something Else"])
        #expect(mainMenu.items.count == 1)
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
