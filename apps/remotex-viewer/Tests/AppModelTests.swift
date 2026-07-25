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

    @Test
    @MainActor
    func keyboardOverridesAppearInactiveForAMacWithoutChangingThePreference() {
        let suiteName = "AppModelTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer {
            defaults.removePersistentDomain(forName: suiteName)
        }

        let model = AppModel(defaults: defaults)
        #expect(model.macOSKeyboardOverridesActive)
        #expect(model.macOSKeyboardOverridesLabel == "Enable macOS Keyboard Overrides")

        var macSession = ViewerSessionState()
        macSession.remoteIsMac = true
        model.apply(session: macSession)

        #expect(!model.macOSKeyboardOverridesActive)
        #expect(model.macOSKeyboardOverridesEnabled)
        #expect(
            model.macOSKeyboardOverridesLabel
                == "macOS Keyboard Overrides (Not Applicable)"
        )
    }
}
