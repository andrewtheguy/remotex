import Foundation
import Testing
@testable import RemotexViewer

/// The two directions of the bridge, spelled out as JSON.
///
/// This is the only place the app and the client agree about anything, and the
/// agreement is by name: a field renamed on one side and not the other compiles on
/// both and does nothing at runtime. So the shapes are pinned here against the
/// literals in `frontend/src/nativeHost.ts`, which is the same arrangement the
/// wire format used to have between Rust and Swift.
struct NativeBridgeTests {
    /// Every command, as the page will receive it.
    ///
    /// The call is a `?.` chain on purpose: the page installs its entry point when
    /// the desktop mounts and removes it when that unmounts, so a menu item pressed
    /// a moment either side of a target switch has to find nothing and do nothing,
    /// rather than throw inside `evaluateJavaScript`.
    @Test
    func everyCommandEncodesTheWayThePageReadsIt() throws {
        let cases: [(NativeCommand, [String: Any])] = [
            (
                .key(
                    NativeKeyEvent(code: "KeyW", pressed: true, caps: false, meta: true)
                ),
                [
                    "type": "key", "code": "KeyW", "pressed": true, "caps": false,
                    "meta": true,
                ]
            ),
            (.releaseInput, ["type": "releaseInput"]),
            (
                .clipboardLocal("hello"),
                ["type": "clipboardLocal", "text": "hello"]
            ),
            (.openClipboard, ["type": "openClipboard"]),
            (.openDisplays, ["type": "openDisplays"]),
            (.closePanel, ["type": "closePanel"]),
            (.resizeToWindow, ["type": "resizeToWindow"]),
            (.setAutoResize(true), ["type": "setAutoResize", "enabled": true]),
            (.selectDisplay(7), ["type": "selectDisplay", "id": 7]),
            (.setAudio(false), ["type": "setAudio", "enabled": false]),
            (
                .setMacKeyOverrides(true),
                ["type": "setMacKeyOverrides", "enabled": true]
            ),
            (.refresh, ["type": "refresh"]),
            (.switchTarget, ["type": "switchTarget"]),
            (.takeOver, ["type": "takeOver"]),
            (
                .sendKeyCombo(["ControlLeft", "KeyR"]),
                ["type": "sendKeyCombo", "codes": ["ControlLeft", "KeyR"]]
            ),
        ]

        for (command, expected) in cases {
            let script = try #require(command.javaScript())
            #expect(
                script.hasPrefix("window.__remotexNative?.command?.("),
                "a command sent to a page with no desktop must do nothing: \(script)"
            )
            #expect(script.hasSuffix(")"))
            let json = String(
                script.dropFirst("window.__remotexNative?.command?.(".count).dropLast()
            )
            let decoded = try #require(
                JSONSerialization.jsonObject(
                    with: Data(json.utf8)
                ) as? [String: Any]
            )
            #expect(
                NSDictionary(dictionary: decoded) == NSDictionary(dictionary: expected),
                "\(command) encoded as \(decoded)"
            )
        }
    }

    /// Clipboard text is somebody else's string, and it goes into a JavaScript
    /// call. Encoded, never interpolated — a remote that copies a closing paren is
    /// not entitled to run anything in this window.
    @Test
    func clipboardTextIsDataRatherThanCode() throws {
        let hostile = #"");window.alert("owned");//"#
        let script = try #require(NativeCommand.clipboardLocal(hostile).javaScript())

        // The text is in there — as an escaped JSON string, which is the point. What
        // must not be in there is the text *as written*, because that is the version
        // that would close the call and start a statement.
        #expect(!script.contains(hostile), "\(script)")
        #expect(script.contains(#"\""#), "the quotes are escaped: \(script)")
        let json = String(
            script.dropFirst("window.__remotexNative?.command?.(".count).dropLast()
        )
        let decoded = try #require(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]
        )
        #expect(decoded["text"] as? String == hostile)
    }

    /// The page's report, decoded from the literal shape `nativeHost.ts` posts.
    @Test
    func theStateEventDecodesEveryFieldTheMenusRead() throws {
        let body: [String: Any] = [
            "type": "state",
            "state": [
                "mode": "desktop",
                "status": "connected",
                "ready": true,
                "size": ["w": 3840, "h": 2160, "scale": 2],
                "canResize": true,
                "canAutoResize": false,
                "autoResize": false,
                "canClipboard": true,
                "canAudio": true,
                "audioEnabled": true,
                "audioError": NSNull(),
                "displays": [
                    ["id": 1, "label": "Built-in", "detail": "1512×982 at 2x"],
                ],
                "activeDisplayId": 1,
                "macKeyOverridesEnabled": true,
                "macKeyOverridesActive": false,
                "remoteIsMac": true,
            ],
        ]

        guard case .state(let state) = try #require(NativeEvent.decode(body)) else {
            Issue.record("expected a state event")
            return
        }
        #expect(state.mode == .desktop)
        #expect(state.status == .connected)
        #expect(state.capturesKeyboard)
        #expect(state.remoteSize == DisplayMode(w: 3840, h: 2160))
        // The remote's own density, which is what the desktop is presented at — a
        // 3840×2160 framebuffer at 2x is a 1920×1080 desktop at full fidelity.
        #expect(state.remoteScale == 2)
        #expect(state.canResize)
        #expect(!state.canAutoResize)
        #expect(state.canClipboard)
        #expect(state.audioEnabled)
        #expect(state.audioError == nil)
        #expect(state.displays == [DisplayChoice(id: 1, label: "Built-in", detail: "1512×982 at 2x")])
        #expect(state.activeDisplayId == 1)
        #expect(state.remoteIsMac)
    }

    /// A page mid-navigation posting half an object leaves the menus reading
    /// "nothing is connected" rather than failing to decode and leaving them
    /// describing a session that ended.
    @Test
    func apartialStateDecodesToNothingConnected() throws {
        let body: [String: Any] = ["type": "state", "state": ["mode": "picker"]]

        guard case .state(let state) = try #require(NativeEvent.decode(body)) else {
            Issue.record("expected a state event")
            return
        }
        #expect(state.mode == .picker)
        #expect(!state.capturesKeyboard)
        #expect(state.size == nil)
    }

    @Test
    func theOtherTwoEventsDecode() {
        #expect(
            NativeEvent.decode(["type": "clipboardFromRemote", "text": "copied"])
                == .clipboardFromRemote("copied")
        )
        #expect(NativeEvent.decode(["type": "unauthenticated"]) == .unauthenticated)
        // Anything else is dropped rather than guessed at: the page and the app
        // ship together, so an unknown event can only be a hand-typed one.
        #expect(NativeEvent.decode(["type": "nonsense"]) == nil)
        #expect(NativeEvent.decode("not an object") == nil)
        #expect(NativeEvent.decode(["type": "clipboardFromRemote"]) == nil)
    }

    /// The readout in the Display menu, which exists to show a density that failed
    /// to apply: two numbers that ought to agree and don't is the whole diagnostic.
    @Test
    func thedisplaySummaryNamesBothDensities() {
        #expect(
            displaySummary(remote: DisplayMode(w: 1920, h: 1080), remoteScale: 1, hostScale: 2)
                == "1920×1080 — remote 1x, this screen 2x"
        )
        #expect(
            displaySummary(remote: nil, remoteScale: 1, hostScale: 2)
                == "Waiting for the Remote Desktop"
        )
        #expect(densityLabel(1.5) == "1.5x")
    }
}
