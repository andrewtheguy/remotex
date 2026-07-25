import SwiftUI

struct SettingsView: View {
    @Bindable var model: AppModel

    var body: some View {
        Form {
            TextField("Gateway", text: $model.gatewayAddress)
                .textFieldStyle(.roundedBorder)
                .onSubmit {
                    model.applyGatewayAddress()
                }
            HStack {
                Spacer()
                Button("Connect") {
                    model.applyGatewayAddress()
                }
                .keyboardShortcut(.defaultAction)
            }

            Divider()

            Toggle(
                "Enable macOS keyboard overrides",
                isOn: $model.macOSKeyboardOverridesEnabled
            )
            .toggleStyle(.checkbox)

            Text(
                "Maps standard Command shortcuts to Control for Windows and Linux guests. "
                    + "Command keys are sent unchanged when disabled."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(20)
        .frame(width: 480)
    }
}
