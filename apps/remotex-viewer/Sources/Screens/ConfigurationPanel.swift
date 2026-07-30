import AppKit
import SwiftUI

/// Edit this instance's `remotex.toml` without leaving the app.
///
/// The same shape as the agent's Settings › Authorized gateways › Manage… panel, for
/// the same reason: the file lives under `~/Library/Application Support`, which is
/// somewhere nobody navigates to by choice, and the question "is what I typed valid"
/// has no answer out there short of restarting something and watching what happens.
///
/// So **Save checks first**. The check is the gateway's own (`check-config`), so what
/// this panel accepts is by construction what the gateway will start on; a refusal
/// keeps the sheet open with the text intact and shows the complaint verbatim, and
/// nothing is written. A clean save restarts the gateway, so a new target is in the
/// picker by the time the sheet closes.
struct ConfigurationPanel: View {
    let store: GatewayConfigStore
    /// Called after a successful save, to restart the gateway on the new config.
    let onSaved: () -> Void

    @Environment(\.dismiss)
    private var dismiss

    @State private var text = ""
    @State private var problem: String?
    @State private var isSaving = false
    @State private var publicKey: String?
    @State private var copiedKey = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Configuration")
                    .font(.title2.weight(.semibold))
                Text("One [[targets]] block per machine. Checked before it is saved.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }

            TextEditor(text: $text)
                .font(.body.monospaced())
                .textEditorStyle(.plain)
                .scrollContentBackground(.hidden)
                .padding(8)
                .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: 8))
                .frame(minHeight: 320)
                .accessibilityLabel("Configuration file")

            if let problem {
                // The gateway's own message, monospaced because it names keys and
                // quotes lines, and selectable because it is the thing somebody will
                // want to search for.
                ScrollView {
                    Text(problem)
                        .font(.caption.monospaced())
                        .foregroundStyle(.orange)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 90)
                .accessibilityLabel("Configuration error")
            }

            publicKeyRow

            HStack {
                Button("Reveal in Finder") {
                    NSWorkspace.shared.activateFileViewerSelecting([store.instance.configURL])
                }
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(isSaving ? "Checking…" : "Save", action: save)
                    .keyboardShortcut(.defaultAction)
                    .disabled(isSaving)
            }
        }
        .padding(20)
        .frame(width: 620)
        .task {
            text = store.read()
            publicKey = await store.publicKey()
        }
    }

    /// This instance's `rxa` public key, which a Mac agent needs in its
    /// `authorized_gateways` before it will answer at all.
    ///
    /// Here because there is nowhere else it could be: the key belongs to the gateway
    /// inside this app, and without it in front of somebody, pairing a Mac would mean
    /// running the bundled binary from a terminal — which is exactly the kind of thing
    /// this app exists to avoid.
    @ViewBuilder
    private var publicKeyRow: some View {
        if let publicKey {
            HStack(spacing: 8) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("This app's gateway key")
                        .font(.callout.weight(.medium))
                    Text(publicKey)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .textSelection(.enabled)
                }
                Spacer()
                Button(copiedKey ? "Copied" : "Copy") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(publicKey, forType: .string)
                    copiedKey = true
                }
            }
            .help("Add this line to a Mac's authorized_gateways to let this app reach it")
        }
    }

    private func save() {
        guard !isSaving else {
            return
        }
        isSaving = true
        Task {
            defer { isSaving = false }
            switch await store.save(text) {
            case .success:
                problem = nil
                dismiss()
                onSaved()
            case .failure(let refusal):
                problem = refusal.message
            }
        }
    }
}
