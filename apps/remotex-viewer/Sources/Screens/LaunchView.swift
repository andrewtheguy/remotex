import SwiftUI

/// The one screen ahead of the target picker: starting the gateway, or saying why it
/// did not start.
///
/// It asks for nothing. There is no address — the gateway is the one in this bundle —
/// and no credentials, so the whole screen is a spinner that sometimes turns into a
/// message. The message is the interesting half: the common failure is a config the
/// gateway refused, so the gateway's own words are shown verbatim, with the two
/// buttons that can act on them.
struct LaunchView: View {
    let model: AppModel
    /// Opens the configuration panel — the fix for almost every failure shown here.
    let editConfiguration: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            if let error = model.launchError {
                failure(error)
            } else {
                starting
            }
            Spacer()
            Text("v\(ProductInfo.version)")
                .font(.footnote)
                .foregroundStyle(.tertiary)
                .padding(.bottom, 12)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var starting: some View {
        VStack(spacing: 14) {
            ProgressView()
                .controlSize(.large)
            Text("Starting \(model.branding)…")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Starting")
    }

    private func failure(_ error: String) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            Label(error, systemImage: "exclamationmark.triangle")
                .font(.headline)
                .foregroundStyle(.orange)
                .multilineTextAlignment(.leading)
                .fixedSize(horizontal: false, vertical: true)

            // The gateway's stderr, monospaced and scrolling. Not summarized and not
            // parsed: this is the text that names the target, the key or the line
            // that is wrong, and any rewriting of it here would hide the useful part.
            if !model.launchLog.isEmpty {
                ScrollView {
                    Text(model.launchLog)
                        .font(.caption.monospaced())
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(8)
                }
                .frame(height: 160)
                .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: 8))
                .accessibilityLabel("Gateway log")
            }

            HStack {
                Button("Configuration…", action: editConfiguration)
                    .disabled(model.config == nil)
                Spacer()
                Button(model.isBusy ? "Starting…" : "Try Again") {
                    Task { await model.relaunchGateway() }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(model.isBusy)
            }
        }
        .frame(width: 460)
        .padding(28)
        .background(.regularMaterial, in: .rect(cornerRadius: 16))
    }
}
