import SwiftUI

/// The connection interstitial, with the same labels and the same two recovery
/// buttons as `frontend/src/RemoteDesktop.tsx`.
///
/// `busy` and `takenOver` both need the user, because resolving either one evicts
/// whoever is on the desktop now. Everything else recovers on its own.
struct StatusOverlayView: View {
    let model: AppModel

    var body: some View {
        VStack(spacing: 12) {
            Text(model.branding)
                .font(.title3)
                .foregroundStyle(.secondary)
            Text(label)
                .font(.title2.weight(.medium))
            if let hint {
                Text(hint)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: 360)
            }
            if let action {
                Button(action) {
                    model.takeOver()
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(32)
        .background(.regularMaterial, in: .rect(cornerRadius: 16))
        .shadow(radius: 20)
    }

    private var label: String {
        switch model.session.connectionStatus {
        case .connecting, .none: "Connecting…"
        case .connected: "Connected"
        case .reconnecting: "Reconnecting…"
        case .busy: "Session in use"
        case .takenOver: "Session taken over"
        }
    }

    private var hint: String? {
        switch model.session.connectionStatus {
        case .busy:
            "This desktop is open in another client."
        case .takenOver:
            "Another client took over this session."
        case .connected where model.session.screen == .desktop:
            // The gap between `connected` and the first `resize`: the engine is
            // up but has not announced a size yet.
            "Waiting for the remote desktop…"
        default:
            nil
        }
    }

    private var action: String? {
        switch model.session.connectionStatus {
        case .busy: "Take Over"
        case .takenOver: "Take It Back"
        default: nil
        }
    }
}
