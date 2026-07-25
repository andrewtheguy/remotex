import SwiftUI

struct RemoteCommands: Commands {
    let model: AppModel

    var body: some Commands {
        CommandMenu("Remote") {
            Button(model.keyboardCaptureEnabled ? "Release Keyboard" : "Capture Keyboard") {
                model.keyboardCaptureEnabled.toggle()
            }
            .keyboardShortcut(.escape, modifiers: [.control, .option, .command])
            .disabled(!model.session.canCaptureKeyboard)

            Divider()

            Button("Synchronize Clipboard") {
                model.clipboard.synchronizeNow()
            }
            .disabled(!model.session.canClipboard)

            Button("Resize to Window") {
                model.resizeToWindow()
            }
            .disabled(!model.session.canResize)

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
