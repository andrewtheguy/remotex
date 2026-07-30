import SwiftUI

/// The post-login target list. Mirrors `frontend/src/TargetPicker.tsx`: name,
/// then `PROTOCOL · host:port`, with every row locked while a connect is in
/// flight and the chosen one saying so.
///
/// A target is picked over the socket (`connect`), not over HTTP — the gateway's
/// session layer owns which target the slot is bound to.
struct TargetPickerView: View {
    @Bindable var model: AppModel

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

            if let busy = model.session.remoteBusy {
                remoteBusy(busy)
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
                        // Under the rows rather than down by Log Out, and inside the
                        // scroll with them: it is a question about the pick being
                        // made — open this one watching — so it belongs where the
                        // picks are. The scroll view takes all the slack this screen
                        // has, so anything below it lands at the foot of the window,
                        // a long way from what it is about.
                        viewOnly
                            .padding(.top, 6)
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

    /// The remote is somebody else's, and this is the one refusal with something to
    /// press. Beside `connectError` rather than through it: that one is a message to
    /// read, this one is a decision, and the whole reason the gateway sends a
    /// distinct message is so a client can offer the button.
    ///
    /// Locked while a connect is in flight for the same reason the rows are — there
    /// is one session slot, so a takeover mid-pick has nowhere to go.
    private func remoteBusy(_ busy: ViewerSessionState.RemoteBusy) -> some View {
        VStack(spacing: 8) {
            Label(
                """
                \(busy.target.isEmpty ? "That target" : busy.target) is in use \
                from \(busy.holder), for \(Self.heldFor(busy.heldSecs)).
                """,
                systemImage: "person.crop.circle.badge.exclamationmark"
            )
            .font(.callout)
            .foregroundStyle(.orange)
            .multilineTextAlignment(.center)
            .fixedSize(horizontal: false, vertical: true)

            Button("Take Over") {
                model.connect(to: busy.target, force: true)
            }
            .disabled(model.session.pendingTarget != nil || busy.target.isEmpty)
        }
    }

    /// "12m" rather than "754s", at the precision a glance wants — the same three
    /// steps the agent's own menu bar and the SPA's picker use for this number.
    static func heldFor(_ seconds: UInt32) -> String {
        if seconds < 60 {
            return "\(seconds)s"
        }
        let minutes = seconds / 60
        if minutes < 60 {
            return "\(minutes)m"
        }
        return "\(minutes / 60)h \(minutes % 60)m"
    }

    /// The same state the desktop's toolbar toggle holds, offered before the pick so
    /// that a session can be *started* watching rather than switched a moment too
    /// late — by which time the target is up and the typing has gone to it. Answered
    /// here, the desktop arrives with capture already suspended and that toolbar
    /// button already lit.
    ///
    /// Deliberately not locked while a connect is in flight, unlike the rows: it is
    /// not a pick, so there is nothing a second answer could contend with, and "did
    /// I mean to drive this one?" is a question that tends to arrive in exactly that
    /// moment.
    private var viewOnly: some View {
        VStack(alignment: .leading, spacing: 4) {
            Toggle("View Only", isOn: $model.isViewOnly)
                .toggleStyle(.checkbox)
            Text("Watch the desktop without sending it anything.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
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
