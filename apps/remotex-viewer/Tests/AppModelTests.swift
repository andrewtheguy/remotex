import Foundation
import Testing
@testable import RemotexViewer

struct AppModelTests {
    @Test
    @MainActor
    func keyboardOverridesDefaultToEnabledAndPersist() {
        let suiteName = "AppModelTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer {
            defaults.removePersistentDomain(forName: suiteName)
        }

        let initial = AppModel(defaults: defaults)
        #expect(initial.macOSKeyboardOverridesEnabled)

        initial.macOSKeyboardOverridesEnabled = false

        let restored = AppModel(defaults: defaults)
        #expect(!restored.macOSKeyboardOverridesEnabled)
    }
}
