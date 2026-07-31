import AppKit
import SwiftUI

/// What this app is: which version, which instance, and which key it pairs with.
///
/// A Mac app's version lives in its About panel, and this one had neither — the
/// version appeared for as long as the launch screen was up and nowhere after it, so
/// the running app could not answer "which build is this" at all. `commandsReplaced`
/// is why: it takes the standard application menu down, About included, and only Quit
/// had been put back (see `RemoteCommands`).
///
/// It is not only the version. An app that carries its own gateway *is* an
/// installation, so the two facts that identify this one belong here beside it: the
/// instance directory everything is read from and written to, and the gateway key a
/// Mac has to authorize. Both are otherwise invisible — the directory is under
/// `~/Library`, and the key exists nowhere but inside the config.
struct AboutPanel: View {
    let branding: String
    /// Nil in an unbundled build, where there is no gateway and so no instance to
    /// describe. The version above still is one, which is the reason this panel does
    /// not require a store to open.
    let store: GatewayConfigStore?

    @Environment(\.dismiss)
    private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 14) {
                // The Dock icon, which for a variant bundle is that variant's own — so
                // this panel says which instance it belongs to twice over.
                //
                // Bound through an optional deliberately: `applicationIconImage` is an
                // implicitly unwrapped `NSImage!`, and an About panel that traps rather
                // than opening is a worse outcome than one with no picture in it.
                if let icon = NSApp.applicationIconImage {
                    Image(nsImage: icon)
                        .resizable()
                        .frame(width: 64, height: 64)
                        .accessibilityHidden(true)
                }

                VStack(alignment: .leading, spacing: 3) {
                    Text(branding)
                        .font(.title2.weight(.semibold))
                    Text("Version \(ProductInfo.version)")
                        .font(.callout)
                        .textSelection(.enabled)
                    // The number that has to match the gateway's, checked on every
                    // launch (`AppModel.launch`). Worth showing next to the version
                    // because it is what a refusal to attach would be about.
                    Text("Wire protocol \(ProductInfo.protocolVersion)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            if let store {
                Divider()
                instance(store)
            }

            Divider()

            HStack {
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 460)
    }

    @ViewBuilder
    private func instance(_ store: GatewayConfigStore) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("Instance")
                .font(.callout.weight(.medium))
            HStack(spacing: 8) {
                // Abbreviated with a tilde, because the useful part of this path is
                // which directory it ends in and the prefix is the same for everybody.
                Text((store.instance.url.path as NSString).abbreviatingWithTildeInPath)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
                Spacer()
                Button("Reveal in Finder") {
                    NSWorkspace.shared.activateFileViewerSelecting([store.instance.configURL])
                }
            }
        }

        GatewayKeyRow(store: store)
    }
}
