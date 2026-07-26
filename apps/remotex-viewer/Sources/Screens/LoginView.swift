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
                        .buttonStyle(.link)
                        .font(.callout)
                        .disabled(model.isBusy)
                    }
                }

                VStack(alignment: .leading, spacing: 6) {
                    Text("Username")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    TextField("", text: $username)
                        .textFieldStyle(.roundedBorder)
                        .textContentType(.username)
                        .focused($focus, equals: .username)
                        .onSubmit { focus = .password }
                    Text("Password")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    SecureField("", text: $password)
                        .textFieldStyle(.roundedBorder)
                        .textContentType(.password)
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
        .onAppear { focus = .username }
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
