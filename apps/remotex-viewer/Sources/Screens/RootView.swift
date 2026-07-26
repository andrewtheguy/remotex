import SwiftUI

/// Which screen is showing, plus the chrome that outlives all of them: the
/// clipboard card and its toolbar button, the window title, and the alert.
struct RootView: View {
    let model: AppModel

    var body: some View {
        ZStack {
            switch model.session.screen {
            case .checking:
                ProgressView()
                    .controlSize(.large)
            case .login:
                LoginView(model: model)
            case .picker, .desktop:
                DesktopScreen(model: model)
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
                get: { model.actionError != nil },
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
                Text(model.actionError ?? "")
            }
        )
        .task {
            await model.start()
        }
    }
}
