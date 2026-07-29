import AppKit
import Foundation
import Testing
@testable import RemotexViewer

/// The target behind the View menu's two resize items.
///
/// It is three lines of forwarding and a `switch`, and every one of them is a
/// silent failure if it is wrong: a renamed selector makes an item that does
/// nothing when clicked, and a swapped case makes one that is greyed while the
/// action behind it would have worked. Neither shows up anywhere but in the menu.
@MainActor
struct ResizeMenuTargetTests {
    /// The items `ViewerMenus` builds have to reach the methods this object
    /// declares. Both sides name the selector as a string — `@objc(…)` here,
    /// `Selector(("…"))` there — so nothing but a test compares them.
    @Test
    func theTargetAnswersTheSelectorsTheMenuItemsSend() {
        let target = ResizeMenuTarget(model: makeModel())

        #expect(target.responds(to: ViewerMenus.resizeToWindowAction))
        #expect(target.responds(to: ViewerMenus.resizeToDisplayAction))
    }

    /// "Resize to Display" is entirely local — the window takes the remote's size
    /// and nothing goes on the wire — so the hook the model calls is the whole of
    /// what forwarding means here.
    @Test
    func resizingToTheDisplayReachesTheModel() throws {
        let model = makeModel()
        var fitted = 0
        model.fitWindowToRemote = { fitted += 1 }
        let target = ResizeMenuTarget(model: model)

        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa"))))
        model.apply(.control(.resize(w: 3200, h: 2000, scale: 2)))
        #expect(model.canResizeToDisplay)

        // Sent the way the menu sends it, rather than by calling the method: this
        // is the half that a mistyped `@objc` name would break. Required first, so
        // a rename fails here rather than aborting the process — `perform` on a
        // selector an object does not answer raises, and an uncaught ObjC
        // exception takes the whole suite down with it.
        try #require(target.responds(to: ViewerMenus.resizeToDisplayAction))
        target.perform(ViewerMenus.resizeToDisplayAction, with: nil)

        #expect(fitted == 1)
    }

    /// And the other direction, which does go on the wire. Driven through the
    /// shared attached-session harness so what is asserted is a real `viewport`
    /// frame on a real socket.
    @Test
    func resizingToTheWindowReachesTheWire() async throws {
        let session = try await AttachedSession.attached(suite: "ResizeMenuTargetTests")
        session.connect(protocolName: "rdp", resize: true)
        let target = ResizeMenuTarget(model: session.model)

        session.model.reportViewport(DisplayMode(w: 1440, h: 900))
        try await session.settle()
        #expect(session.viewports.isEmpty, "measuring is not asking")

        try #require(target.responds(to: ViewerMenus.resizeToWindowAction))
        target.perform(ViewerMenus.resizeToWindowAction, with: nil)

        try await session.expectViewport(w: 1440, h: 900)
    }

    /// Enablement comes from `validateMenuItem`, which AppKit calls as the menu
    /// opens. Each action has to read its *own* property: swapping them would grey
    /// the wrong item, and on RDP — where both are allowed — nothing would look
    /// wrong at all.
    @Test
    func eachItemIsValidatedAgainstItsOwnProperty() {
        // The row where the two answers differ: `rxa` without `resize` cannot be
        // asked to match the window, but its desktop holds still and can be
        // fitted to. See `AppModelTests.onlyAFollowingRemoteCannotBeFittedTo`.
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rxa", resize: false))))
        model.apply(.control(.resize(w: 1920, h: 1080, scale: 1)))
        model.reportViewport(DisplayMode(w: 1600, h: 900))
        #expect(!model.canResizeNow)
        #expect(model.canResizeToDisplay)

        let target = ResizeMenuTarget(model: model)

        #expect(!target.validateMenuItem(item(ViewerMenus.resizeToWindowAction)))
        #expect(target.validateMenuItem(item(ViewerMenus.resizeToDisplayAction)))
    }

    /// The same pair on a target that allows both, so a `validateMenuItem` stuck
    /// at `false` cannot pass the test above by accident.
    @Test
    func bothItemsGoLiveOnATargetThatAllowsBoth() {
        let model = makeModel()
        model.apply(.status(.connected))
        model.apply(.control(.connected(connected(protocolName: "rdp", resize: true))))
        model.apply(.control(.resize(w: 1920, h: 1080, scale: 1)))
        model.reportViewport(DisplayMode(w: 1600, h: 900))

        let target = ResizeMenuTarget(model: model)

        #expect(target.validateMenuItem(item(ViewerMenus.resizeToWindowAction)))
        #expect(target.validateMenuItem(item(ViewerMenus.resizeToDisplayAction)))
    }

    /// Off a session both are dead, which is the state the menu bar comes up in.
    @Test
    func neitherItemIsLiveWithoutADesktop() {
        let target = ResizeMenuTarget(model: makeModel())

        #expect(!target.validateMenuItem(item(ViewerMenus.resizeToWindowAction)))
        #expect(!target.validateMenuItem(item(ViewerMenus.resizeToDisplayAction)))
    }

    /// This object is a menu item's target, and AppKit asks it about whatever item
    /// it is sent. Anything that is not one of ours is not this object's to grey —
    /// answering `false` by default would disable an item it knows nothing about.
    @Test
    func anItemThatIsNotOursIsLeftEnabled() {
        let target = ResizeMenuTarget(model: makeModel())

        #expect(target.validateMenuItem(item(#selector(NSWindow.toggleFullScreen(_:)))))
        #expect(target.validateMenuItem(NSMenuItem()))
    }

    private func item(_ action: Selector) -> NSMenuItem {
        NSMenuItem(title: "", action: action, keyEquivalent: "")
    }

    private func makeModel() -> AppModel {
        AppModel(
            defaults: UserDefaults(suiteName: "ResizeMenuTargetTests.\(UUID().uuidString)")!,
            clipboard: ClipboardSynchronizer(
                pasteboard: NSPasteboard.withUniqueName(),
                startsPolling: false
            )
        )
    }

    private func connected(
        protocolName: String,
        resize: Bool = false
    ) -> ServerMessage.Connected {
        ServerMessage.Connected(
            name: "mac",
            protocolName: protocolName,
            resize: resize,
            clipboard: false,
            audio: false
        )
    }
}
