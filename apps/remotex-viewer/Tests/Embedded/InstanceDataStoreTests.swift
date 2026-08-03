import Foundation
import Testing
@testable import RemotexViewer

/// Which WebKit data store an instance's page gets.
///
/// It matters because the client's remembered preferences live in that store's
/// `localStorage`, and WebKit keeps the store in this app's container rather than
/// in the instance directory — so the identifier is the only thing making
/// `--instance-dir` isolate preferences the way it isolates the config and log.
struct InstanceDataStoreTests {
    /// Stable across launches, or every launch is a fresh set of preferences.
    @Test
    func theSameDirectoryAlwaysGetsTheSameStore() {
        let one = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex-qa"))
        let again = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex-qa"))

        #expect(one.dataStoreIdentifier == again.dataStoreIdentifier)
        // And the same directory spelled a longer way is the same instance, which
        // is what it is to everything else here.
        let indirect = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/./remotex-qa"))
        #expect(one.dataStoreIdentifier == indirect.dataStoreIdentifier)
    }

    /// Different per instance, which is the whole point of deriving it from the
    /// directory: a QA run must not read or write the real instance's preferences.
    @Test
    func adifferentDirectoryGetsADifferentStore() {
        let real = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex"))
        let qa = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex-qa"))

        #expect(real.dataStoreIdentifier != qa.dataStoreIdentifier)
    }

    /// WebKit takes a `UUID` and is entitled to a well-formed one.
    @Test
    func theIdentifierIsAWellFormedUuid() {
        let bytes = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex"))
            .dataStoreIdentifier.uuid

        #expect(bytes.6 & 0xF0 == 0x40, "version 4")
        #expect(bytes.8 & 0xC0 == 0x80, "variant 1")
    }
}
