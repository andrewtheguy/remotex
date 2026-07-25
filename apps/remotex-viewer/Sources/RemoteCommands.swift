import SwiftUI

struct RemoteCommands: Commands {
    @Bindable var model: AppModel

    var body: some Commands {
        CommandMenu("Remote") {
            Button(model.keyboardCaptureEnabled ? "Release Keyboard" : "Capture Keyboard") {
                model.keyboardCaptureEnabled.toggle()
            }
            .keyboardShortcut(.escape, modifiers: [.control, .option, .command])
            .disabled(!model.session.canCaptureKeyboard)

            Toggle(
                "Enable macOS Keyboard Overrides",
                isOn: $model.macOSKeyboardOverridesEnabled
            )

            Divider()

            Button("Synchronize Clipboard") {
                model.clipboard.synchronizeNow()
            }
            .disabled(!model.session.canClipboard)

            Button("Resize to Window") {
                model.resizeToWindow()
            }
            .disabled(!model.session.canResize)

            // The web floating menu is hidden while the viewer is attached, so
            // without this a target whose only resize path is a fixed list
            // (the Mac agent on a virtual display) could not be resized at all.
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

            Button("Log Out") {
                model.logout()
            }
            .disabled(
                model.session.screen != .picker
                    && model.session.screen != .desktop
            )
        }
    }
}
