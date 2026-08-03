import Foundation
import Testing
@testable import RemotexViewer

struct ProductInfoTests {
    /// There is no version to pin any more — the bundle's is the workspace's, put
    /// there by `build-viewer-app.sh`, and there is no wire protocol here to keep in
    /// step with the gateway's, because the two ship in one bundle. What is left is
    /// that a build that is *not* bundled says so rather than naming a release it
    /// is not.
    @Test
    func anUnbundledBuildClaimsNoRelease() {
        // These tests run unbundled, so this is that path.
        #expect(ProductInfo.version == "0.0.0-unbundled")
    }
}
