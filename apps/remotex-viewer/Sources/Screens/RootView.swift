import SwiftUI

/// Which screen is showing, plus the chrome that outlives all of them: the
/// clipboard card, the window title, and the alert.
///
/// No toolbar. The window's title bar carries nothing but the title: every
/// action it could hold already lives in a menu — the clipboard card is opened
/// from **Remote › Clipboard…** — and a button duplicated into the title bar is
/// one more thing to keep in step with the state that enables it.
struct RootView: View {
    let model: AppModel

    /// Whether the configuration sheet is up. Held here rather than on the model
    /// because it is a fact about this window, not about the session — and both the
    /// launch screen and the Remote menu open the same one.
    @State private var isEditingConfiguration = false
    /// Whether the About panel is up. Same reasoning, and its own flag rather than one
    /// enum of "which sheet": the two are opened from different places and neither ever
    /// replaces the other.
    @State private var isShowingAbout = false

    var body: some View {
        ZStack {
            switch model.session.screen {
            case .home:
                HomeView(model: model)
            case .login:
                LoginView(model: model)
            case .launching:
                LaunchView(model: model) { isEditingConfiguration = true }
            case .picker, .desktop:
                DesktopScreen(model: model)
            }
        }
        .sheet(isPresented: $isEditingConfiguration) {
            // `canEditConfiguration` and not merely `config != nil`: this panel edits
            // the file `serve-embedded` reads, and Save restarts that gateway. Against
            // a remote one it would be editing a config nothing on screen is running
            // from — see `AppModel.canEditConfiguration`.
            if model.canEditConfiguration, let config = model.config {
                ConfigurationPanel(store: config) {
                    Task { await model.relaunchGateway() }
                }
            }
        }
        // The Remote menu's item is an AppKit-side action on the model, so the sheet
        // it wants is opened by watching the model rather than by calling into this
        // view — which nothing outside SwiftUI can do.
        .onChange(of: model.configurationRequests) { _, _ in
            isEditingConfiguration = true
        }
        // Chained rather than a second `.sheet` on the same view: each modifier wraps
        // the previous one's result, which is what keeps two presentations on one view
        // from contending for the same slot.
        .sheet(isPresented: $isShowingAbout) {
            // The instance is a fact about this app whichever gateway it is talking
            // to — it is where these preferences and this log live — so it is always
            // shown.
            AboutPanel(
                branding: model.branding,
                store: model.config
            )
        }
        .onChange(of: model.aboutRequests) { _, _ in
            isShowingAbout = true
        }
        .overlay(alignment: .bottomTrailing) {
            if model.clipboard.isPresented {
                ClipboardCard(clipboard: model.clipboard)
                    .padding(20)
            }
        }
        .navigationTitle(model.windowTitle)
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
        // No launch `.task` here: the gateway is started by the scene
        // (`RemotexViewerApp`), which is where the model's lifetime is.
    }
}
