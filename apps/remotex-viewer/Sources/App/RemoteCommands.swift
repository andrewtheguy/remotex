import SwiftUI

struct RemoteCommands: Commands {
    @Bindable var model: AppModel

    var body: some Commands {
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
            .keyboardShortcut("r", modifiers: [.command, .shift])
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

            // Both steps back out of the session, and neither carries a key
            // equivalent. They had one so the way in — server, login, picker —
            // could be walked from the keyboard, which was only ever for driving
            // the app in a test: on a painting desktop `KeyboardCapture` takes
            // every Command chord bar Quit, Close and Settings and sends it to the
            // remote, so the shortcut worked on some screens and silently typed
            // into the guest on others. A shortcut that depends on which screen is
            // up is worse than none.
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

            // The server step's Continue answers to Return already, but only while
            // the window is key and nothing else has eaten it. On the menu it is
            // reachable no matter where focus sits, which is what a keyboard-only
            // pass — or a script driving one — needs.
            Button("Connect to Gateway") {
                Task { await model.connectToGateway() }
            }
            .keyboardShortcut(.return, modifiers: .command)
            .disabled(
                model.session.screen != .server
                    || model.isBusy
                    || model.gatewayAddress.isEmpty
            )
        }
    }
}
