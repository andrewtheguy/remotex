import SwiftUI

/// Step two: who you are.
///
/// Only ever reached for a remote gateway — the embedded one has no login to offer
/// (`no_login_handler` in src/server.rs) and never lands here. The address was
/// already validated by the home screen, so a failure here can only be about the
/// credentials, which is why the address is shown but not editable. **Change** goes
/// back to step one.
struct LoginView: View {
    let model: AppModel

    @State private var username = ""
    @State private var password = ""
    @FocusState private var focus: Field?

    private enum Field {
        case username
        case password
    }

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(alignment: .leading, spacing: 20) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(model.branding)
                        .font(.largeTitle.weight(.semibold))
                    HStack(spacing: 6) {
                        Text(model.chosen?.label ?? "")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Button("Change") {
                            Task { await model.changeGateway() }
                        }
                        // Escape, as going back a step is everywhere else.
                        .keyboardShortcut(.cancelAction)
                        .buttonStyle(.link)
                        .font(.callout)
                        .disabled(model.isBusy)
                    }
                }

                VStack(alignment: .leading, spacing: 6) {
                    Text("Username")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    // The captions above are separate Text views, so without these
                    // VoiceOver reaches two unnamed fields. No placeholder instead:
                    // it would show as grey sample text inside an empty field.
                    // No `textContentType`, on either field, and that is the whole
                    // point: `.username` and `.password` broke Command-V here. Once
                    // macOS's Passwords panel had appeared over this form, paste
                    // beeped in *both* fields and went on beeping after the panel
                    // was gone — it takes key window and the responder chain does
                    // not come back from it, so the Edit menu's `paste:` had nothing
                    // to act on, and the paste that works everywhere else in the app
                    // (see `ViewerMenus`) was broken again here, durably, for anyone
                    // with a saved login.
                    //
                    // Nothing is given up by dropping them: the Passwords panel
                    // still offers itself over these fields — macOS reads a secure
                    // field and its neighbour on its own — and paste now survives
                    // it. The content types added the breakage and not the offer,
                    // which is not what their names suggest, so this is a comment
                    // rather than a line someone tidies back in. The gateway address
                    // on the home screen keeps `.URL`; it was never part of this.
                    TextField("", text: $username)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityLabel("Username")
                        .focused($focus, equals: .username)
                        .onSubmit { focus = .password }
                    Text("Password")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    SecureField("", text: $password)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityLabel("Password")
                        .focused($focus, equals: .password)
                        .onSubmit(signIn)
                    if let error = model.loginError {
                        Text(error)
                            .font(.callout)
                            .foregroundStyle(.red)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }

                HStack {
                    Spacer()
                    Button(model.isBusy ? "Signing In…" : "Sign In", action: signIn)
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.isBusy || username.isEmpty || password.isEmpty)
                }
            }
            .frame(width: 360)
            .padding(28)
            .background(.regularMaterial, in: .rect(cornerRadius: 16))
            Spacer()
            Text("v\(ProductInfo.version)")
                .font(.footnote)
                .foregroundStyle(.tertiary)
                .padding(.bottom, 12)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        // Deferred a turn, as on the home screen: set straight from `onAppear` the
        // assignment is dropped, and then the username is typed into whatever had
        // focus instead — or into the password field, which is what Tab reaches
        // first from nowhere.
        .onAppear {
            DispatchQueue.main.async { focus = .username }
        }
    }

    private func signIn() {
        guard !model.isBusy, !username.isEmpty, !password.isEmpty else {
            return
        }
        Task {
            await model.logIn(username: username, password: password)
        }
    }
}
