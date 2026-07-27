import SwiftUI

/// Step two: who you are.
///
/// The gateway was already validated by the server step, so a failure here can
/// only be about the credentials — which is why the address is shown but not
/// editable. **Change** goes back to step one.
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
                        Text(model.gateway.url.absoluteString)
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
                        // This link is the only way back to the server step; the
                        // screen it is on is half of `changeGateway`'s own guard,
                        // so all that is left to say here is not mid-request.
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
                    TextField("", text: $username)
                        .textFieldStyle(.roundedBorder)
                        .textContentType(.username)
                        .accessibilityLabel("Username")
                        .focused($focus, equals: .username)
                        .onSubmit { focus = .password }
                    Text("Password")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    SecureField("", text: $password)
                        .textFieldStyle(.roundedBorder)
                        .textContentType(.password)
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
        // Deferred a turn, as on the server step: set straight from `onAppear` the
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
