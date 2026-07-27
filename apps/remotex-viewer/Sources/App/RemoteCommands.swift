import SwiftUI

struct RemoteCommands: Commands {
    @Bindable var model: AppModel

    var body: some Commands {
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

            // Both steps back out of the session.
            Button("Log Out") {
                Task { await model.logOut() }
            }
            .disabled(
                model.session.screen != .picker
                    && model.session.screen != .desktop
            )

            // Signed out only, which is why it sits under Log Out: from the picker
            // or the desktop that is the step to take first.
            Button("Change Gateway") {
                Task { await model.changeGateway() }
            }
            .disabled(!model.canChangeGateway)

            // The server step's own Continue button is the way in; this is the same
            // action on the menu, reachable no matter where focus sits.
            Button("Connect to Gateway") {
                Task { await model.connectToGateway() }
            }
            .disabled(
                model.session.screen != .server
                    || model.isBusy
                    || model.gatewayAddress.isEmpty
            )
        }
    }
}
