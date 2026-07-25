import Foundation
import Testing
@testable import RemotexViewer

/// The state payload is the contract between the served frontend
/// (`frontend/src/nativeHost.ts`) and this viewer. Both ship together, so the
/// interesting question is not versioning but which fields are load-bearing:
/// a missing capability flag must reject the update, while a malformed entry
/// in a list must not take the rest of the state down with it.
struct NativeBridgeTests {
    private static func desktopState(
        overrides: [String: Any] = [:],
        removing: [String] = []
    ) -> [String: Any] {
        var value: [String: Any] = [
            "screen": "desktop",
            "connectionStatus": "connected",
            "connectedTarget": "mac",
            "remoteIsMac": true,
            "displayModes": [["w": 1280, "h": 960], ["w": 1024, "h": 768]],
            "remoteSize": ["w": 1024, "h": 768],
            "canResize": false,
            "canClipboard": true,
            "canCaptureKeyboard": true,
        ]
        for key in removing {
            value.removeValue(forKey: key)
        }
        return value.merging(overrides) { _, new in new }
    }

    @Test
    @MainActor
    func aDesktopStateDecodesWithItsResolutionMenu() throws {
        let state = try #require(NativeBridge.decodeState(Self.desktopState()))
        #expect(state.screen == .desktop)
        #expect(state.remoteIsMac)
        #expect(state.displayModes == [DisplayMode(w: 1280, h: 960), DisplayMode(w: 1024, h: 768)])
        #expect(state.remoteSize == DisplayMode(w: 1024, h: 768))
    }

    // Every target but a Mac agent on a virtual display sends no menu at all.
    @Test
    @MainActor
    func aTargetWithoutAMenuDecodesToAnEmptyOne() throws {
        let value = Self.desktopState(removing: ["displayModes", "remoteSize"])
        let state = try #require(NativeBridge.decodeState(value))
        #expect(state.displayModes.isEmpty)
        #expect(state.remoteSize == nil)
    }

    @Test
    @MainActor
    func aMalformedModeIsDroppedRatherThanRejectingTheState() throws {
        let value = Self.desktopState(overrides: [
            "displayModes": [["w": 1280, "h": 960], ["w": "wide"], "1024x768"],
            "remoteSize": ["h": 768],
        ])
        let state = try #require(NativeBridge.decodeState(value))
        #expect(state.displayModes == [DisplayMode(w: 1280, h: 960)])
        #expect(state.remoteSize == nil)
        #expect(state.canClipboard, "the rest of the state survives one bad entry")
    }

    // A capability flag is not optional: defaulting a missing one would hand the
    // viewer a menu item that silently does nothing.
    @Test
    @MainActor
    func aMissingCapabilityFlagRejectsTheWholeState() {
        for field in ["screen", "remoteIsMac", "canResize", "canClipboard", "canCaptureKeyboard"] {
            let value = Self.desktopState(removing: [field])
            #expect(NativeBridge.decodeState(value) == nil, "\(field) must be required")
        }
    }
}
