import SwiftUI

struct ClipboardCard: View {
    @Bindable var clipboard: ClipboardSynchronizer
    @FocusState private var editorFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label("Clipboard", systemImage: "doc.on.clipboard")
                    .font(.headline)
                Spacer()
                Button {
                    clipboard.closePanel()
                } label: {
                    Image(systemName: "xmark")
                }
                .buttonStyle(.borderless)
                .keyboardShortcut(.cancelAction)
                .help("Close clipboard")
                .accessibilityLabel("Close clipboard")
            }

            if clipboard.isEditing {
                TextEditor(text: $clipboard.draft)
                    .font(.body.monospaced())
                    .textEditorStyle(.plain)
                    .focused($editorFocused)
                    .frame(minHeight: 132)
                    .padding(8)
                    .background(
                        Color(nsColor: .textBackgroundColor),
                        in: RoundedRectangle(cornerRadius: 8)
                    )
                    .overlay {
                        RoundedRectangle(cornerRadius: 8)
                            .stroke(.separator)
                    }
                    .accessibilityLabel("Clipboard text")
            } else {
                concealedSummary
            }

            if let unavailableMessage = clipboard.unavailableMessage {
                Label(unavailableMessage, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }

            HStack {
                if !clipboard.isEditing {
                    // There is nothing to reveal when the remote's clipboard was
                    // refused for its size; the same button then opens an empty
                    // editor, so it says so.
                    Button(clipboard.oversizedBytes == nil ? "Reveal" : "Type instead") {
                        clipboard.reveal()
                    }
                }
                Button("Send") {
                    clipboard.sendDraft()
                }
                .disabled(
                    !clipboard.isEditing
                        || clipboard.draft.isEmpty
                        || clipboard.isOverByteLimit
                )
                .help(
                    clipboard.isOverByteLimit
                        ? "Too long: \(clipboard.draftByteCount) bytes; the limit is \(ClipboardSynchronizer.maximumBytes)"
                        : "Send the editable draft to the remote"
                )
                Button("Copy") {
                    clipboard.copy()
                }
                Spacer()
            }

            HStack(alignment: .firstTextBaseline) {
                if clipboard.isEditing {
                    Text(
                        "\(clipboard.draftByteCount) / \(ClipboardSynchronizer.maximumBytes) bytes"
                    )
                    .foregroundStyle(clipboard.isOverByteLimit ? .red : .secondary)
                }
                Spacer()
                // Transient: the notice appears and clears itself, so VoiceOver
                // has to poll it. Its own text is the announcement, which is
                // why it carries no separate label.
                Text(clipboard.notice ?? "")
                    .foregroundStyle(.secondary)
                    .accessibilityAddTraits(.updatesFrequently)
            }
            .font(.caption)
            .frame(minHeight: 16)
        }
        .padding(16)
        .frame(width: 390)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
        .overlay {
            RoundedRectangle(cornerRadius: 14)
                .stroke(.separator)
        }
        .shadow(color: .black.opacity(0.28), radius: 18, y: 8)
        .onAppear {
            editorFocused = clipboard.isEditing
        }
        .onChange(of: clipboard.isEditing) { _, isEditing in
            editorFocused = isEditing
        }
    }

    private var concealedSummary: some View {
        VStack(alignment: .leading, spacing: 7) {
            // No CRC32 for a clipboard that was never transferred — there are no
            // bytes here to checksum. Its size and the limit are the whole story.
            if let oversizedBytes = clipboard.oversizedBytes {
                Text("LEN \(oversizedBytes)B")
                Text("LIMIT \(ClipboardSynchronizer.maximumBytes)B")
                Text("AT \(clipboard.activityDescription ?? "UNKNOWN")")
                Text("TOO LARGE TO TRANSFER")
                    .foregroundStyle(.orange)
            } else {
                Text("CRC32 \(clipboard.concealedCRC32 ?? "00000000")")
                Text("LEN \(clipboard.concealedByteCount ?? 0)B")
                Text("AT \(clipboard.activityDescription ?? "UNKNOWN")")
            }
        }
        .font(.body.monospaced())
        .textSelection(.disabled)
        .frame(maxWidth: .infinity, minHeight: 132, alignment: .leading)
        .padding(12)
        .background(.black.opacity(0.16), in: RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            clipboard.oversizedBytes == nil
                ? "Concealed remote clipboard summary"
                : "Remote clipboard too large to transfer"
        )
    }
}
