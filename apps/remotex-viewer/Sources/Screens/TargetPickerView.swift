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
                        ForEach(Array(model.targets.enumerated()), id: \.element.id) {
                            index, target in
                            row(target, ordinal: index + 1)
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

    /// `ordinal` is the row's 1-based place in the list, which is also its
    /// shortcut: ⌘1 picks the first target, ⌘2 the second. Nine of them, because
    /// ⌘0 is not the tenth of anything — past that the list needs the mouse.
    ///
    /// Shown on the row as well as bound, so a keyboard-only pass over this screen
    /// does not have to be memorised.
    private func row(_ target: TargetInfo, ordinal: Int) -> some View {
        let pending = model.session.pendingTarget
        let isPending = pending == target.name
        let shortcut = (1 ... 9).contains(ordinal)
            ? KeyboardShortcut(KeyEquivalent(Character("\(ordinal)")), modifiers: .command)
            : nil
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
                if isPending {
                    Text("Connecting…")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                } else if shortcut != nil {
                    Text("⌘\(ordinal)")
                        .font(.callout.monospacedDigit())
                        .foregroundStyle(.tertiary)
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(.rect)
        }
        .keyboardShortcut(shortcut)
        .buttonStyle(.plain)
        .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: 10))
        // Every row locks, not just the chosen one: there is one session slot,
        // so a second pick mid-connect has nowhere to go.
        .disabled(pending != nil)
        // `.plain` gives a disabled button no treatment of its own, so the lock
        // would be invisible. Dimmed as `.picker-target:disabled` is in the SPA.
        .opacity(pending == nil ? 1 : 0.6)
        .accessibilityLabel("\(target.name), \(target.detail)")
        // The "Connecting…" line is inside the label VoiceOver replaces, so the
        // one row that is doing something has to say so somewhere else.
        .accessibilityValue(isPending ? "Connecting…" : "")
    }
}
