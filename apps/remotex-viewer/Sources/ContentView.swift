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
        .overlay(alignment: .bottomTrailing) {
            if model.clipboard.isPresented {
                ClipboardCard(clipboard: model.clipboard)
                    .padding(20)
            }
        }
        .navigationTitle(model.windowTitle)
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    model.clipboard.togglePanel()
                } label: {
                    HStack(spacing: 6) {
                        if model.clipboard.isFetching {
                            ProgressView()
                                .controlSize(.small)
                        } else {
                            Image(systemName: "doc.on.clipboard")
                        }
                        Text("Clipboard")
                    }
                }
                .disabled(
                    !model.clipboard.isEnabled
                        || model.clipboard.isFetching
                )
                .help(
                    model.clipboard.isEnabled
                        ? "Read and write the remote clipboard"
                        : "Clipboard integration is not connected"
                )
                .accessibilityLabel("Clipboard")
                .accessibilityValue(
                    model.clipboard.isFetching ? "Fetching" : ""
                )
            }
        }
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
