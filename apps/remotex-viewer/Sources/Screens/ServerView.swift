import SwiftUI

/// Step one: which gateway, and can it be spoken to.
///
/// A step of its own rather than a field on the login form, because it answers a
/// different question — reachable, and speaking a protocol this build knows —
/// and it answers it when asked. Nothing is validated on launch or as a side
/// effect of signing in, so a bad address is reported here, before any
/// credentials have been typed.
struct ServerView: View {
    @Bindable var model: AppModel

    @FocusState private var focused: Bool

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(alignment: .leading, spacing: 20) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Connect to a gateway")
                        .font(.title.weight(.semibold))
                    Text("The address of the remotex server.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }

                VStack(alignment: .leading, spacing: 6) {
                    TextField(
                        "https://remotex.example.com",
                        text: $model.gatewayAddress
                    )
                    .textFieldStyle(.roundedBorder)
                    .textContentType(.URL)
                    .focused($focused)
                    .disabled(model.isBusy)
                    .onSubmit(connect)
                    if let error = model.gatewayError {
                        Label(error, systemImage: "exclamationmark.triangle")
                            .font(.callout)
                            .foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }

                HStack {
                    Spacer()
                    Button(model.isBusy ? "Connecting…" : "Continue", action: connect)
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.isBusy || model.gatewayAddress.isEmpty)
                }
            }
            .frame(width: 360)
            .padding(28)
            .background(.regularMaterial, in: .rect(cornerRadius: 16))
            Spacer()
            Text("v\(ProductInfo.version)")
                .font(.footnote)
                .foregroundStyle(.tertiary)
                .padding(.bottom, 12)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        // Deferred a turn rather than set straight from `onAppear`: assigning focus
        // while SwiftUI is still installing the view is dropped, and this screen is
        // re-entered — "Change Gateway" comes back to it — so the field would have
        // no focus and typing an address would go nowhere.
        .onAppear {
            DispatchQueue.main.async { focused = true }
        }
    }

    private func connect() {
        guard !model.isBusy, !model.gatewayAddress.isEmpty else {
            return
        }
        Task {
            await model.connectToGateway()
        }
    }
}
