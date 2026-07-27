import SwiftUI

/// Which screen is showing, plus the chrome that outlives all of them: the
/// clipboard card and its toolbar button, the window title, and the alert.
struct RootView: View {
    @Bindable var model: AppModel

    var body: some View {
        ZStack {
            switch model.session.screen {
            case .server:
                ServerView(model: model)
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
            // Left of the Clipboard button, and only on the desktop: off it there
            // is no input to hold back, and the two belong together because the
            // clipboard is one of the paths this closes — turning it on is what
            // greys the button beside it out.
            if model.session.screen == .desktop {
                ToolbarItem(placement: .primaryAction) {
                    Toggle(isOn: $model.isViewOnly) {
                        HStack(spacing: 6) {
                            // Filled as well as highlighted: the button style's own
                            // "on" background is the whole difference otherwise, and
                            // it is a subtle one on a compact bar.
                            Image(systemName: model.isViewOnly ? "eye.fill" : "eye")
                            Text("View Only")
                        }
                    }
                    .toggleStyle(.button)
                    .help(
                        model.isViewOnly
                            ? "View only: the keyboard and pointer stay on this Mac"
                            : "Watch without sending anything to the remote, "
                                + "leaving this Mac its own shortcuts"
                    )
                    .accessibilityLabel("View Only")
                }
            }

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
                .help(clipboardHelp)
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
        // Deliberately no launch `.task`: the gateway is contacted when the user
        // presses Continue, not on appear.
    }

    /// Three reasons the button can be disabled, and view only is the one the user
    /// just chose — so it is named rather than left looking like a target without
    /// clipboard support.
    private var clipboardHelp: String {
        if model.isViewOnly {
            "The clipboard is not shared while view only"
        } else if model.clipboard.isEnabled {
            "Read and write the remote clipboard"
        } else {
            "Clipboard integration is not connected"
        }
    }
}
