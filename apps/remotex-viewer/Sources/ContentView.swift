import SwiftUI

struct ContentView: View {
    let model: AppModel

    var body: some View {
        ZStack {
            WebViewContainer(model: model)

            switch model.bridgeStatus {
            case .incompatible(let message), .failed(let message):
                bridgeError(message)
            case .loading, .ready:
                EmptyView()
            }
        }
        .navigationTitle(model.windowTitle)
        .alert(
            "remotex",
            isPresented: Binding(
                get: { model.navigationError != nil },
                set: { showing in
                    if !showing {
                        model.clearError()
                    }
                }
            ),
            actions: {
                Button("OK") {
                    model.clearError()
                }
            },
            message: {
                Text(model.navigationError ?? "")
            }
        )
    }

    private func bridgeError(_ message: String) -> some View {
        VStack(spacing: 16) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 36))
                .foregroundStyle(.orange)
            Text("Viewer integration unavailable")
                .font(.title2)
            Text(message)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 440)
            Button("Reload") {
                model.reload()
            }
            .keyboardShortcut(.defaultAction)
        }
        .padding(28)
        .background(.regularMaterial, in: .rect(cornerRadius: 16))
        .shadow(radius: 20)
    }
}
