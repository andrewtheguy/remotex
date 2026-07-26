import SwiftUI

/// Sign in, and choose which gateway to sign in to.
///
/// The Server field is here rather than in a Settings window because the address
/// and the credentials are one decision — and because a gateway that cannot be
/// reached, or speaks a protocol this build does not, is a thing to say next to
/// the field that caused it rather than in a floating error card.
struct LoginView: View {
    @Bindable var model: AppModel

    @State private var username = ""
    @State private var password = ""
    @FocusState private var focus: Field?

    private enum Field {
        case server
        case username
        case password
    }

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(alignment: .leading, spacing: 20) {
                Text(model.branding)
                    .font(.largeTitle.weight(.semibold))

                VStack(alignment: .leading, spacing: 6) {
                    Text("Server")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    TextField("https://remotex.example.com", text: $model.gatewayAddress)
                        .textFieldStyle(.roundedBorder)
                        .textContentType(.URL)
                        .focused($focus, equals: .server)
                        .onSubmit { focus = .username }
                    if let error = model.gatewayError {
                        Label(error, systemImage: "exclamationmark.triangle")
                            .font(.callout)
                            .foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
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
        .onAppear {
            focus = model.gatewayError == nil ? .username : .server
        }
    }

    private func signIn() {
        guard !username.isEmpty, !password.isEmpty else {
            return
        }
        Task {
            await model.logIn(username: username, password: password)
        }
    }
}
