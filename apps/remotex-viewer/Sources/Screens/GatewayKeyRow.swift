import AppKit
import SwiftUI

/// This instance's `rxa` public key, with a Copy button.
///
/// The one value in the app that has to leave it: a Mac running `remotex-agent`
/// answers no gateway whose public key is absent from its `authorized_gateways`, so
/// without this on screen pairing would mean running the bundled binary from a
/// terminal — which is the sort of thing this app exists to remove.
///
/// A view of its own because it appears in two places for two different reasons: in
/// the configuration panel, beside the target being paired, and in About, as part of
/// what this instance *is*. Both ask the same store the same question, so they ask it
/// through the same view rather than each keeping its own copy of the answer.
struct GatewayKeyRow: View {
    let store: GatewayConfigStore?

    @State private var key: String?
    @State private var isLoaded = false
    @State private var copied = false

    var body: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 2) {
                Text("This app's gateway key")
                    .font(.callout.weight(.medium))
                // Three states, all of them said out loud. The row used to be hidden
                // whenever the key could not be read, which is the one case where
                // somebody is looking straight at it and needs to be told why it is
                // not there — an absent row reads as a feature that does not exist.
                Text(detail)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
            }
            Spacer()
            if let key {
                Button(copied ? "Copied" : "Copy") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(key, forType: .string)
                    copied = true
                }
            }
        }
        .help("Add this line to a Mac's authorized_gateways to let this app reach it")
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Gateway key")
        .task {
            key = await store?.publicKey()
            isLoaded = true
        }
    }

    private var detail: String {
        if let key {
            key
        } else if !isLoaded {
            "Reading…"
        } else {
            // Only reachable when the config lost its `[rxa]` section or the key
            // could not be minted on first launch, so it names the section rather
            // than apologising: adding one is the fix, and Configuration… is where.
            "No [rxa].private_key in this instance's configuration yet"
        }
    }
}
