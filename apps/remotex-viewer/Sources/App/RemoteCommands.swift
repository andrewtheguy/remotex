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

            Button(
                model.clipboard.isFetching
                    ? "Fetching Clipboard…"
                    : "Clipboard…"
            ) {
                model.clipboard.togglePanel()
            }
            .disabled(!model.clipboard.isEnabled || model.clipboard.isFetching)

            Button("Resize to Window") {
                model.resizeToWindow()
            }
            .disabled(!model.canResizeNow)

            // A target whose only resize path is a fixed list — the Mac agent on
            // a virtual display — has no other way to be resized.
            Menu("Resolution") {
                ForEach(model.session.displayModes) { mode in
                    Button {
                        model.setResolution(mode)
                    } label: {
                        if mode == model.session.remoteSize {
                            Label(mode.label, systemImage: "checkmark")
                        } else {
                            Text(mode.label)
                        }
                    }
                }
            }
            .disabled(model.session.displayModes.isEmpty)

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
    }
}
