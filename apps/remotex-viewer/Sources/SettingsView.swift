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
        }
        .padding(20)
        .frame(width: 480)
    }
}
