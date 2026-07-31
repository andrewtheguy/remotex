import Foundation
import Testing
@testable import RemotexViewer

/// The config store, over an injected validator.
///
/// The validator is the gateway's own `check-config` in the app; here it is a closure,
/// because what these tests are about is what the store *does* with a verdict — write
/// or refuse, and leave the file alone either way. What counts as valid is settled on
/// the Rust side, where the parser lives (`check_config_agrees_with_what_the_gateway_would_do`).
@MainActor
struct GatewayConfigStoreTests {
    /// The promise the whole panel rests on: a refused edit changes nothing on disk.
    /// Not a backup, not a partial write — the config the gateway reads is always one
    /// it would start on.
    @Test
    func aRefusedSaveWritesNothing() async throws {
        let directory = try ScratchDirectory()
        try directory.write("remotex.toml", "# the original\n")
        let store = store(in: directory, verdict: .failure(ConfigProblem("unknown field `hostname`")))

        let result = await store.save("[[targets]]\nhostname = \"x\"\n")

        guard case .failure(let problem) = result else {
            Issue.record("the save was expected to be refused")
            return
        }
        // The gateway's own words, unrewritten: they name the key to fix.
        #expect(problem.message == "unknown field `hostname`")
        #expect(directory.contents(of: "remotex.toml") == "# the original\n")
    }

    @Test
    func anAcceptedSaveReplacesTheFile() async throws {
        let directory = try ScratchDirectory()
        try directory.write("remotex.toml", "# the original\n")
        let store = store(in: directory, verdict: .success(()))

        let result = await store.save("# the new one\n")

        // Matched rather than compared: `Result<Void, _>` is not `Equatable`, because
        // `Void` is not.
        if case .failure(let problem) = result {
            Issue.record("the save was refused: \(problem.message)")
        }
        #expect(directory.contents(of: "remotex.toml") == "# the new one\n")
    }

    /// The file holds every target's password, so it must not be readable by anything
    /// else on the machine — and a write that replaces it must not widen the mode back
    /// (which is what a plain `Data.write` to a fresh path would do).
    @Test
    func theSavedConfigIsOwnerOnly() async throws {
        let directory = try ScratchDirectory()
        let store = store(in: directory, verdict: .success(()))

        _ = await store.save("# first\n")
        #expect(try mode(of: directory.instance.configURL) == 0o600)
        _ = await store.save("# second\n")
        #expect(try mode(of: directory.instance.configURL) == 0o600, "a replacement must not widen it")
    }

    /// A first launch has to produce a config, and one with this instance's own `rxa`
    /// identity in it — otherwise the one protocol written for this app could not be
    /// configured without a terminal.
    @Test
    func aFirstLaunchWritesATemplateWithAMintedKey() async throws {
        let directory = try ScratchDirectory()
        let store = store(in: directory, verdict: .success(()), value: "rxgs-minted-key")

        try await store.bootstrapIfNeeded()

        let text = try #require(directory.contents(of: "remotex.toml"))
        #expect(text.contains("private_key = \"rxgs-minted-key\""), "got: \(text)")
        // There is an example target to copy, and it is commented out — so the picker
        // offers nothing rather than a machine that does not exist. `[server]` and
        // `[[targets]]` are both *mentioned*, in prose, which is the point of checking
        // the live lines rather than the text: the only table this file declares is
        // `[rxa]`, and anything else would be a config the gateway refuses.
        #expect(text.contains("# [[targets]]"), "an example to copy")
        let liveTables = text
            .split(separator: "\n", omittingEmptySubsequences: false)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { $0.hasPrefix("[") }
        #expect(liveTables == ["[rxa]"], "got: \(liveTables)")
    }

    /// Bootstrapping twice must not overwrite the targets somebody added — it is run
    /// on every launch.
    @Test
    func bootstrappingAgainLeavesAnExistingConfigAlone() async throws {
        let directory = try ScratchDirectory()
        try directory.write("remotex.toml", "# mine\n")
        let store = store(in: directory, verdict: .success(()), value: "rxgs-minted-key")

        try await store.bootstrapIfNeeded()

        #expect(directory.contents(of: "remotex.toml") == "# mine\n")
    }

    /// A missing config reads as the template rather than as empty, so the editor
    /// opens on something explanatory instead of a blank sheet.
    @Test
    func readingWithNoFileYetGivesTheTemplate() throws {
        let directory = try ScratchDirectory()
        let store = store(in: directory, verdict: .success(()))
        #expect(store.read() == GatewayConfigStore.template)
    }

    /// The pairing value a Mac agent needs. Read through the binary, because it is
    /// derived from the private key and only the gateway can derive it.
    @Test
    func thePublicKeyComesFromTheGateway() async throws {
        let directory = try ScratchDirectory()
        let paired = store(in: directory, verdict: .success(()), value: "rxap-public")
        #expect(await paired.publicKey() == "rxap-public")

        let unpaired = store(in: directory, verdict: .success(()), value: nil)
        #expect(await unpaired.publicKey() == nil, "no identity yet")
    }

    /// Deriving the key costs a gateway process, and three places on screen ask for
    /// it — the picker most often, since it is shown again after every target switch.
    /// So a successful answer is remembered, and only a save can retire it.
    @Test
    func thePublicKeyIsDerivedOnceAndAgainAfterASave() async throws {
        let directory = try ScratchDirectory()
        let reads = Counter()
        let store = GatewayConfigStore(
            instance: directory.instance,
            validate: { _ in .success(()) },
            readValue: { _ in
                await reads.increment()
                return "rxap-public"
            }
        )

        #expect(await store.publicKey() == "rxap-public")
        #expect(await store.publicKey() == "rxap-public")
        #expect(await reads.count == 1, "the second read came from the cache")

        // A saved config may carry a different identity, so the app must not go on
        // showing the key it had.
        _ = await store.save("# a new config\n")
        #expect(await store.publicKey() == "rxap-public")
        #expect(await reads.count == 2, "a save retires it")
    }

    /// A read that failed is not remembered: the config may simply have no identity
    /// yet — bootstrapping is what mints one — and caching `nil` would leave the row
    /// saying so for the life of the app.
    @Test
    func aFailedReadIsAskedAgain() async throws {
        let directory = try ScratchDirectory()
        let reads = Counter()
        let store = GatewayConfigStore(
            instance: directory.instance,
            validate: { _ in .success(()) },
            readValue: { _ in
                await reads.increment()
                return nil
            }
        )

        #expect(await store.publicKey() == nil)
        #expect(await store.publicKey() == nil)
        #expect(await reads.count == 2)
    }

    /// The bundle names the instance, which is what lets a stamped-out variant be a
    /// separate installation with no argument to pass — LaunchServices gives a
    /// double-clicked app none. The shipped bundle is `remotex`, so this must leave
    /// that one exactly where it was.
    @Test
    func theInstanceIsNamedAfterTheBundle() {
        #expect(InstanceDirectory.instanceName(ofBundleNamed: "remotex") == "remotex")
        #expect(InstanceDirectory.instanceName(ofBundleNamed: "remotex-work") == "remotex-work")
        #expect(InstanceDirectory.instanceName(ofBundleNamed: "  remotex-work  ") == "remotex-work")
    }

    /// An unbundled build has no `CFBundleName` at all — `swift test` is one, and so is
    /// anything run out of `.build` — and still has to look somewhere.
    @Test
    func aBundleWithNoNameFallsBackToTheDefault() {
        #expect(InstanceDirectory.instanceName(ofBundleNamed: nil) == "remotex")
        #expect(InstanceDirectory.instanceName(ofBundleNamed: "") == "remotex")
        #expect(InstanceDirectory.instanceName(ofBundleNamed: "   ") == "remotex")
    }

    /// Whatever the bundle is called, the result is one visible path component: a `/`
    /// would put this instance inside another directory, and a leading `.` would hide
    /// the folder somebody is expected to be able to open.
    @Test
    func theNameIsAlwaysOneVisibleComponent() {
        #expect(InstanceDirectory.instanceName(ofBundleNamed: "remotex/work") == "remotex-work")
        #expect(InstanceDirectory.instanceName(ofBundleNamed: "remotex:work") == "remotex-work")
        #expect(InstanceDirectory.instanceName(ofBundleNamed: ".hidden") == "hidden")
        #expect(InstanceDirectory.instanceName(ofBundleNamed: "...") == "remotex")
    }

    // MARK: - Helpers

    private func store(
        in directory: ScratchDirectory,
        verdict: Result<Void, ConfigProblem>,
        value: String? = nil
    ) -> GatewayConfigStore {
        GatewayConfigStore(
            instance: directory.instance,
            validate: { _ in verdict },
            readValue: { _ in value }
        )
    }

    private func mode(of url: URL) throws -> Int {
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        return (attributes[.posixPermissions] as? NSNumber)?.intValue ?? 0
    }
}

/// How many times an injected closure was called. An actor because the closures the
/// store takes are `@Sendable` and may run anywhere.
private actor Counter {
    private(set) var count = 0

    func increment() {
        count += 1
    }
}
