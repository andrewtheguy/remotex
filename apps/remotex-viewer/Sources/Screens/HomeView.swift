import SwiftUI

/// Step one: which gateway.
///
/// The app carries a gateway, and for a Mac sitting on the same network as its
/// targets that is the whole answer — no address, no login, nothing to keep in sync.
/// It is the wrong answer over a slow link, where every tile crosses that link twice
/// and the gateway would do better living beside the target with this app talking to
/// it. Nothing here can tell which case somebody is in, so this screen asks rather
/// than deciding, and it asks on every launch with the last answer already selected:
/// Return is enough to repeat it.
struct HomeView: View {
    @Bindable var model: AppModel

    @FocusState private var addressFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(alignment: .leading, spacing: 20) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(model.branding)
                        .font(.largeTitle.weight(.semibold))
                    Text("Choose the gateway this session runs on.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }

                // A Picker rather than two buttons: these are two answers to one
                // question, only one can be in force, and which one is remembered —
                // all of which a segmented control says and a pair of buttons does
                // not.
                Picker("Gateway", selection: $model.prefersRemoteGateway) {
                    Text("On This Mac").tag(false)
                    Text("Somewhere Else").tag(true)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                // The one case where there is no choice to make. An unbundled build
                // has no `remotex-gateway` beside it, so the local option would fail
                // at the point of being pressed; `AppModel.init` has already selected
                // the other one.
                .disabled(model.gateway == nil)
                .accessibilityLabel("Gateway")

                if model.prefersRemoteGateway {
                    remote
                } else {
                    local
                }

                if let error = model.homeError {
                    Label(error, systemImage: "exclamationmark.triangle")
                        .font(.callout)
                        .foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }

                HStack {
                    Spacer()
                    Button(model.isBusy ? "Connecting…" : "Continue", action: proceed)
                        .keyboardShortcut(.defaultAction)
                        .disabled(!canProceed)
                }
            }
            .frame(width: 400)
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
        // re-entered — "Change Gateway…" comes back to it — so the field would have
        // no focus and typing an address would go nowhere.
        .onAppear {
            DispatchQueue.main.async { addressFocused = model.prefersRemoteGateway }
        }
        .onChange(of: model.prefersRemoteGateway) { _, remote in
            addressFocused = remote
        }
    }

    private var local: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(
                model.gateway == nil
                    ? "This copy of remotex is incomplete: it has no gateway to run."
                    : "Runs a gateway inside this app. Nothing to configure and nothing to sign in to — this Mac connects to the targets itself."
            )
            .font(.callout)
            .foregroundStyle(model.gateway == nil ? .orange : .secondary)
            .fixedSize(horizontal: false, vertical: true)
        }
        // Two blocks of different heights swapping in one place makes the card jump.
        // A floor under both is cheaper than laying the pair out and hiding one.
        .frame(minHeight: 64, alignment: .top)
    }

    private var remote: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("The address of a remotex gateway to sign in to.")
                .font(.callout)
                .foregroundStyle(.secondary)
            TextField("https://remotex.example.com", text: $model.gatewayAddress)
                .textFieldStyle(.roundedBorder)
                .textContentType(.URL)
                .focused($addressFocused)
                .disabled(model.isBusy)
                .onSubmit(proceed)
                .accessibilityLabel("Gateway address")
        }
        .frame(minHeight: 64, alignment: .top)
    }

    private var canProceed: Bool {
        guard !model.isBusy else {
            return false
        }
        if model.prefersRemoteGateway {
            return !model.gatewayAddress.trimmingCharacters(in: .whitespaces).isEmpty
        }
        return model.gateway != nil
    }

    private func proceed() {
        guard canProceed else {
            return
        }
        Task { await model.chooseGateway() }
    }
}
