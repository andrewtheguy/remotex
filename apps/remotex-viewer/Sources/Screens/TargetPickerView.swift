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
            // The instance's name, then what this screen is for — the same pair, in
            // the same order, as the SPA's `picker-brand` and its `<h1>`. Deliberately
            // the smaller and quieter of the two, as it is there: the branding is what
            // this is, and "Pick a target" is only what to do about it.
            VStack(spacing: 4) {
                Text(model.branding)
                    .font(.largeTitle.weight(.semibold))
                    .accessibilityAddTraits(.isHeader)
                Text("Pick a target")
                    .font(.headline)
                    .foregroundStyle(.secondary)
            }
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
                // A first launch lands here, so this is not only an empty state but
                // the app's front door: the sentence says what is missing and the
                // button below is where it gets added.
                VStack(spacing: 6) {
                    Text("No machines are configured yet.")
                        .foregroundStyle(.secondary)
                    Text("Add one in Configuration, below.")
                        .font(.callout)
                        .foregroundStyle(.tertiary)
                }
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
            // What there is to do when nothing here is connectable yet, at the foot of
            // the screen — and it is not the same list for the two gateways, which is
            // why this branches rather than greying items out.
            VStack(spacing: 12) {
                Divider()
                defaultsFooter
                if model.usesEmbeddedGateway {
                    localFooter
                } else {
                    remoteFooter
                }
            }
            .frame(width: 420)
            .padding(.bottom, 20)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(.background)
    }

    /// The two remembered defaults, shown here as the one place they can be set
    /// before a target is picked — and the same values the Remote and View menus'
    /// live controls edit mid-session. Both gateways show them: they are the
    /// client's own preferences, not the gateway's, so they sit above the branch
    /// that differs between the two.
    ///
    /// "if compatible" is a fixed caption, not a per-target check: the picker never
    /// learns a target's capabilities (the target list carries none), so each is an
    /// intent, applied on connect only where `connected` reports the target can
    /// honour it.
    @ViewBuilder
    private var defaultsFooter: some View {
        VStack(alignment: .leading, spacing: 6) {
            Toggle(
                "Auto-resize the remote to the window, if compatible",
                isOn: Binding(
                    get: { model.autoResizeByDefault },
                    set: { model.autoResizeByDefault = $0 }
                )
            )
            Toggle(
                "Play the remote's sound, if compatible",
                isOn: Binding(
                    get: { model.audioByDefault },
                    set: { model.audioByDefault = $0 }
                )
            )
        }
        .toggleStyle(.checkbox)
        .font(.callout)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// The two things to do when the local gateway's list is empty: add a machine,
    /// and hand this gateway's key to one.
    ///
    /// The key is here rather than only in About and the configuration sheet because
    /// this is the screen a first launch lands on and the screen a target switch comes
    /// back to — and a value somebody has to copy onto another Mac is no use two
    /// panels deep.
    @ViewBuilder
    private var localFooter: some View {
        GatewayKeyRow(store: model.config)
        Button("Configuration…") {
            model.editConfiguration()
        }
        .disabled(!model.canEditConfiguration)
    }

    /// Neither of those is this screen's business against a gateway somewhere else.
    ///
    /// The **key** is the one that would actively mislead: it identifies the gateway
    /// in this bundle to a Mac agent, and against a remote gateway nothing here is
    /// going to present it — the connection to the target is made by that gateway,
    /// with its own key, which is the one a target has to authorize. Copying this one
    /// onto a Mac would authorize a machine that is never going to call. The
    /// **configuration** goes for the same reason one step further along: the targets
    /// on this screen came out of the remote gateway's config, and this app's own file
    /// is not where they are edited.
    ///
    /// What is left is the pair that *is* true here: which gateway these targets came
    /// from, and the way back to choose another.
    @ViewBuilder
    private var remoteFooter: some View {
        if let location = model.chosen?.location {
            Text(location.url.absoluteString)
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
                .accessibilityLabel("Gateway")
        }
        Button("Change Gateway…") {
            Task { await model.changeGateway() }
        }
        .disabled(!model.canChangeGateway)
    }

    /// The remote is somebody else's, and this is the one refusal with something to
    /// press. Beside `connectError` rather than through it: that one is a message to
    /// read, this one is a decision, and the whole reason the gateway sends a
    /// distinct message is so a client can offer the button.
    ///
    /// Locked while a connect is in flight for the same reason the rows are — there
    /// is one session slot, so a takeover mid-pick has nowhere to go.
    private func remoteBusy(_ busy: ViewerSessionState.RemoteBusy) -> some View {
        let target = busy.target.isEmpty ? "that target" : busy.target
        // Two situations, and they must not read the same. Being refused is about a
        // request this user made; being taken over is about one they did not, and
        // saying "in use" to somebody whose desktop just vanished describes the
        // wrong event entirely. Only the remote's session went either way — the
        // login and this gateway's slot are still theirs, which is why the target
        // list is right here.
        let message =
            busy.takenOver
            ? "Your session on \(target) was taken over from \(busy.holder)."
            : "\(target.prefix(1).uppercased() + target.dropFirst()) is in use "
                + "from \(busy.holder), for \(Self.heldFor(busy.heldSecs))."
        return VStack(spacing: 8) {
            Label(message, systemImage: "person.crop.circle.badge.exclamationmark")
                .font(.callout)
                .foregroundStyle(.orange)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)

            Button(busy.takenOver ? "Take It Back" : "Take Over") {
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
