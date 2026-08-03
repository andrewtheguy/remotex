import SwiftUI

/// Which screen is showing, plus the chrome that outlives both: the window title,
/// the sheets, and the alert.
///
/// Two screens. Either the gateway is coming up — or explaining why it did not —
/// or the client is on screen and owns everything inside it.
///
/// No toolbar. The window's title bar carries nothing but the title: every action
/// it could hold already lives in a menu, and a button duplicated into the title
/// bar is one more thing to keep in step with the state that enables it.
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
            switch model.screen {
            case .launching:
                LaunchView(model: model) { isEditingConfiguration = true }
            case .ready(let gateway):
                RemoteWebHost(
                    model: model,
                    gateway: gateway,
                    // The toolbar gives way once there is a desktop behind it, and
                    // not before: the picker is an ordinary page and wants the
                    // window to look like a window.
                    hidesToolbar: model.state.mode == .desktop
                )
                .ignoresSafeArea(edges: .bottom)
            }
        }
        .sheet(isPresented: $isEditingConfiguration) {
            if let config = model.config {
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
            AboutPanel(branding: model.branding, store: model.config)
        }
        .onChange(of: model.aboutRequests) { _, _ in
            isShowingAbout = true
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
        // The gateway comes up with the window. There is nothing to ask first any
        // more — one gateway, in this bundle, with a token nobody types.
        .task {
            await model.launch()
        }
    }
}
