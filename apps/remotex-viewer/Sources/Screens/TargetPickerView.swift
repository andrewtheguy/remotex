import SwiftUI

/// The post-login target list. Mirrors `frontend/src/TargetPicker.tsx`: name,
/// then `PROTOCOL · host:port`, with every row locked while a connect is in
/// flight and the chosen one saying so.
///
/// A target is picked over the socket (`connect`), not over HTTP — the gateway's
/// session layer owns which target the slot is bound to.
struct TargetPickerView: View {
    let model: AppModel

    var body: some View {
        VStack(spacing: 0) {
            Text(model.branding)
                .font(.largeTitle.weight(.semibold))
                .padding(.top, 32)

            if let error = model.session.connectError {
                Label(error, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 16)
                    .frame(maxWidth: 420)
            }

            if model.targets.isEmpty {
                Text("No targets are configured on this gateway.")
                    .foregroundStyle(.secondary)
                    .padding(.top, 24)
            } else {
                ScrollView {
                    VStack(spacing: 10) {
                        ForEach(model.targets) { target in
                            row(target)
                        }
                    }
                    .frame(width: 420)
                    .padding(.vertical, 24)
                }
            }

            Spacer()
            Button("Log Out") {
                Task { await model.logOut() }
            }
            .padding(.bottom, 20)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(.background)
    }

    private func row(_ target: TargetInfo) -> some View {
        let pending = model.session.pendingTarget
        return Button {
            model.connect(to: target.name)
        } label: {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text(target.name)
                        .font(.headline)
                    Text(target.detail)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if pending == target.name {
                    Text("Connecting…")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: 10))
        // Every row locks, not just the chosen one: there is one session slot,
        // so a second pick mid-connect has nowhere to go.
        .disabled(pending != nil)
        .accessibilityLabel("\(target.name), \(target.detail)")
    }
}
