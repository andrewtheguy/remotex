import Foundation
import Testing
@testable import RemotexViewer

/// Where an instance's Chromium keeps its profile.
///
/// It matters because the client's three remembered preferences live in that
/// profile's `localStorage`. Under WebKit this was the fiddliest part of the app —
/// `WKWebsiteDataStore` keeps its store in the app's own container whatever you ask
/// of it, so the instance directory had to be hashed into a `UUID` and handed over
/// as an identifier just to make `--instance-dir` isolate preferences the way it
/// isolates the config and the log. Chromium takes a path, so the whole mechanism
/// is a subdirectory now, and these tests are what say so.
struct InstanceProfileTests {
    /// Inside the instance, which is what makes `--instance-dir` isolate the
    /// preferences at all.
    @Test
    func theProfileLivesInsideTheInstanceDirectory() {
        let instance = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex-qa"))

        #expect(instance.browserProfile.path.hasPrefix("/tmp/remotex-qa/"))
    }

    /// Stable across launches, or every launch is a fresh set of preferences.
    @Test
    func theSameDirectoryAlwaysGetsTheSameProfile() {
        let one = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex-qa"))
        let again = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex-qa"))

        #expect(one.browserProfile == again.browserProfile)
        // And the same directory spelled a longer way is the same instance, which
        // is what it is to everything else here.
        let indirect = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/./remotex-qa"))
        #expect(one.browserProfile == indirect.browserProfile)
    }

    /// Different per instance: a QA run must not read or write the real instance's
    /// preferences.
    @Test
    func aDifferentDirectoryGetsADifferentProfile() {
        let real = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex"))
        let qa = InstanceDirectory(url: URL(fileURLWithPath: "/tmp/remotex-qa"))

        #expect(real.browserProfile != qa.browserProfile)
    }
}
