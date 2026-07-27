import AppKit
import SwiftUI

struct RemoteCommands: Commands {
    @Bindable var model: AppModel

    var body: some Commands {
        // `commandsReplaced` takes the standard app menu down with the rest, Quit
        // included, and an app you can only leave by closing its window is not one.
        // Put back deliberately, and — like every item below — without a chord: the
        // Command-Q the muscle memory reaches for belongs to the guest while a
        // desktop is focused, so advertising it on this item would be advertising a
        // shortcut that does something else. View only, or moving focus off the
        // desktop, is what makes the menu reachable from the keyboard again.
        CommandGroup(replacing: .appTermination) {
            Button("Quit remotex") {
                NSApp.terminate(nil)
            }
        }

        // Deliberately nothing for full screen. AppKit adds Enter Full Screen to a
        // View menu of its own and does not give the group up when it is replaced
        // here — claiming `.sidebar` left the app with two items for the one action —
        // so its item is left where it is, and `ViewerApplicationDelegate` takes the
        // Control-Command-F off it. AppKit's flips its own title between Enter and
        // Exit from window state, which is more than an item declared here could say.

        // No item here carries a key equivalent, and that is a rule rather than an
        // omission. While the desktop is painting and focused, `KeyboardCapture`
        // takes every Command chord the system delivers and sends it to the remote —
        // so a shortcut on this menu fires only on the screens where nothing is
        // captured, and types into the guest on the one where the item usually
        // matters. The ones that were here existed to drive the app from the keyboard
        // in a test, which is not reason enough to ship a chord whose meaning depends
        // on which screen is up. View only does not change that: it suspends capture,
        // so a chord would work while it is on and type into the guest while it is
        // off, which is the same dependence with another name.
        CommandMenu("Remote") {
            Toggle(
                model.macOSKeyboardOverridesLabel,
                isOn: Binding(
                    get: { model.macOSKeyboardOverridesActive },
                    set: { model.macOSKeyboardOverridesEnabled = $0 }
                )
            )
            .disabled(model.session.remoteIsMac)

            Divider()

            // Also a toolbar button, and this is the copy that survives: the toolbar
            // gives way to the desktop while one is showing, so on the screen where
            // view only means anything the button is not on screen.
            Toggle("View Only", isOn: $model.isViewOnly)
                .disabled(model.session.screen != .desktop)

            Button(
                model.clipboard.isFetching
                    ? "Fetching Clipboard…"
                    : "Clipboard…"
            ) {
                model.clipboard.togglePanel()
            }
            .disabled(!model.clipboard.isEnabled || model.clipboard.isFetching)

            // The two directions a size mismatch can be settled, and exactly one of
            // them is ever the one that can move: a remote that takes a size from
            // here (RDP with `resize`) gets the first, and every other target gets
            // the second, which resizes this window and sends nothing. The greyed
            // one stays in the menu on purpose — which way a target allows is worth
            // reading off the pair rather than inferring from an item that is not
            // there.
            Button("Resize to Window") {
                model.resizeToWindow()
            }
            .disabled(!model.canResizeNow)

            Button("Resize to Display") {
                model.resizeToDisplay()
            }
            .disabled(!model.canResizeToDisplay)

            // The escape hatch for a framebuffer that has gone wrong: re-announce
            // the size and repaint everything.
            Button("Refresh") {
                model.refresh()
            }
            .disabled(model.session.screen != .desktop)

            Button("Switch Target") {
                model.switchTarget()
            }
            .disabled(model.session.screen != .desktop)

            if model.session.connectionStatus == .busy {
                Button("Take Over Session") {
                    model.takeOver()
                }
            } else if model.session.connectionStatus == .takenOver {
                Button("Take Session Back") {
                    model.takeOver()
                }
            }

            Divider()

            // The way out of a session, and the last item on this menu.
            //
            // Nothing here for the way *in*. Connect to Gateway went first — the
            // server step's own Continue button under another name — and Change
            // Gateway followed it for the same reason: the only screen it was ever
            // enabled on is the login step, which shows the address with a Change
            // link beside it. A menu item that can only fire while the button that
            // does the same thing is on screen is a second name for that button.
            Button("Log Out") {
                Task { await model.logOut() }
            }
            .disabled(
                model.session.screen != .picker
                    && model.session.screen != .desktop
            )
        }

        // Which of the remote's screens to look at, one checkable item each.
        //
        // A menu of its own rather than a submenu under Remote, because it is the
        // one control here that is used *during* a session and more than once —
        // buried a level down it would be a worse version of the browser's
        // floating panel. `ViewerApplicationDelegate` keeps a list of the
        // top-level menus this app may have, and this title has to be on it.
        //
        // Only `rxa` fills it: RDP and VNC each deliver one framebuffer spanning
        // every remote screen, so there is nothing to choose between. The menu
        // stays rather than disappearing — a menu bar whose items come and go is
        // harder to learn than one item that is sometimes greyed — and says why.
        CommandMenu("Display") {
            if model.session.displays.isEmpty {
                Button("No Displays to Choose From") {}
                    .disabled(true)
            } else {
                ForEach(model.session.displays) { display in
                    Toggle(
                        "\(display.label) — \(display.detail)",
                        isOn: Binding(
                            get: { display.id == model.session.activeDisplayID },
                            // Only ever set to true, by picking the item: the
                            // remote is always sharing exactly one display, so
                            // there is no "off" to honour. Unticking the active
                            // item asks for nothing and `selectDisplay` drops it.
                            set: { _ in model.selectDisplay(display.id) }
                        )
                    )
                }
            }
        }
    }
}
