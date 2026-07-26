import AppKit
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

    @Test
    @MainActor
    func clipboardPushPayloadRequiresTextAndANullableTimestamp() {
        #expect(
            NativeBridge.decodeRemoteClipboardPush([
                "type": "remoteClipboard",
                "text": "copied",
                "changedAtMs": 1_725_000_123_456,
            ])
                == RemoteClipboardPush(
                    text: "copied",
                    changedAtMs: 1_725_000_123_456
                )
        )
        #expect(
            NativeBridge.decodeRemoteClipboardPush([
                "type": "remoteClipboard",
                "text": "old",
                "changedAtMs": NSNull(),
            ])
                == RemoteClipboardPush(text: "old", changedAtMs: nil)
        )

        for malformed: [String: Any] in [
            ["text": "missing timestamp"],
            ["changedAtMs": NSNull()],
            ["text": 7, "changedAtMs": NSNull()],
            ["text": "fractional", "changedAtMs": 1.5],
            ["text": "boolean", "changedAtMs": true],
            ["text": "negative", "changedAtMs": -1],
            // 2^63: the nearest Double to Int64.max, and one past it.
            ["text": "unrepresentable", "changedAtMs": 9_223_372_036_854_775_808.0],
        ] {
            #expect(NativeBridge.decodeRemoteClipboardPush(malformed) == nil)
        }
    }

    @Test
    @MainActor
    func fetchResultPayloadRequiresARequestIdTextAndNullableTimestamp() {
        #expect(
            NativeBridge.decodeClipboardFetchResult([
                "type": "clipboardFetchResult",
                "requestId": "viewer-request",
                "text": "fetched",
                "changedAtMs": NSNull(),
            ])
                == ClipboardFetchResult(
                    requestID: "viewer-request",
                    text: "fetched",
                    changedAtMs: nil
                )
        )
        #expect(
            NativeBridge.decodeClipboardFetchResult([
                "type": "clipboardFetchResult",
                "requestId": "viewer-request",
                "text": "",
                "changedAtMs": 0,
            ])
                == ClipboardFetchResult(
                    requestID: "viewer-request",
                    text: "",
                    changedAtMs: 0
                )
        )

        // The failure answer: nothing was read, so there is no timestamp.
        #expect(
            NativeBridge.decodeClipboardFetchResult([
                "type": "clipboardFetchResult",
                "requestId": "viewer-request",
                "text": NSNull(),
            ])
                == ClipboardFetchResult(
                    requestID: "viewer-request",
                    text: nil,
                    changedAtMs: nil
                )
        )

        for malformed: [String: Any] in [
            ["text": "missing id", "changedAtMs": NSNull()],
            ["requestId": "", "text": "empty id", "changedAtMs": NSNull()],
            ["requestId": 9, "text": "bad id", "changedAtMs": NSNull()],
            ["requestId": "id", "changedAtMs": NSNull()],
            ["requestId": "id", "text": "missing timestamp"],
            ["requestId": "id", "text": "bad timestamp", "changedAtMs": "now"],
        ] {
            #expect(NativeBridge.decodeClipboardFetchResult(malformed) == nil)
        }
    }

    // Whether a request id is current is panel state, so decoding cannot judge
    // it: a stale answer has to arrive intact and be refused a layer up (see
    // `requestMatchingTimeoutAndCloseResetThePanel`).
    @Test
    @MainActor
    func aStaleFetchRequestIdStillDecodes() throws {
        let stale = try #require(
            NativeBridge.decodeClipboardFetchResult([
                "requestId": "stale-id",
                "text": "stale",
                "changedAtMs": NSNull(),
            ])
        )
        #expect(stale.requestID == "stale-id")
        #expect(stale.text == "stale")
        #expect(stale.changedAtMs == nil)
    }
}
