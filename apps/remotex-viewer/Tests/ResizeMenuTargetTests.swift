import AppKit
import Foundation
import Testing
@testable import RemotexViewer

/// The target behind the View menu's three resize items.
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
        let target = ResizeMenuTarget(model: AppModel.underTest(sink: RecordingSink()))

        #expect(target.responds(to: ViewerMenus.autoResizeAction))
        #expect(target.responds(to: ViewerMenus.resizeToWindowAction))
        #expect(target.responds(to: ViewerMenus.resizeToDisplayAction))
    }

    /// The mode item toggles, and its tick is set by `validateMenuItem` — AppKit
    /// asks as the menu opens, so that is where the answer has to be right. A tick
    /// pushed on at click time instead would be correct only until something else
    /// changed the mode.
    ///
    /// The tick follows the *page*, not the click, which is the whole reason it is
    /// read this way round: the client decides whether it is following the window,
    /// and a menu that ticked itself would claim a mode the client had refused.
    @Test
    func theAutoResizeItemTogglesAndCarriesItsOwnTick() throws {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)
        model.apply(
            .state(AppModel.desktopState(canResize: true, canAutoResize: true))
        )
        let target = ResizeMenuTarget(model: model)
        let auto = item(ViewerMenus.autoResizeAction)

        #expect(target.validateMenuItem(auto))
        #expect(auto.state == .off)

        try #require(target.responds(to: ViewerMenus.autoResizeAction))
        target.perform(ViewerMenus.autoResizeAction, with: nil)
        #expect(sink.sent(.setAutoResize(true)))

        // The client did it, and says so; now the tick moves and the one-shots go.
        model.apply(
            .state(
                AppModel.desktopState(canResize: true, canAutoResize: true, autoResize: true)
            )
        )
        #expect(model.autoResizes)
        #expect(target.validateMenuItem(auto))
        #expect(auto.state == .on)
        #expect(
            !target.validateMenuItem(item(ViewerMenus.resizeToWindowAction)),
            "and the one-shots it governs are greyed while it is on"
        )
        #expect(!target.validateMenuItem(item(ViewerMenus.resizeToDisplayAction)))

        // Sent again: the same item switches back, so the menu is never one-way.
        sink.clear()
        target.perform(ViewerMenus.autoResizeAction, with: nil)
        #expect(sink.sent(.setAutoResize(false)))
    }

    /// The state this item exists in on RDP and Apple Screen Sharing: greyed while
    /// the two one-shots stay live, and saying so in its title — greying alone
    /// would read as "this session cannot resize", which is exactly what the item
    /// below it disproves.
    @Test
    func theAutoResizeItemIsGreyedAndLabelledWhereOnlyAskingIsAllowed() {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)
        model.apply(
            .state(AppModel.desktopState(canResize: true, canAutoResize: false))
        )
        let target = ResizeMenuTarget(model: model)
        let auto = item(ViewerMenus.autoResizeAction)

        #expect(!target.validateMenuItem(auto))
        #expect(auto.title == "Auto Resize (Not Applicable)")
        #expect(
            target.validateMenuItem(item(ViewerMenus.resizeToWindowAction)),
            "asking for one resize still works"
        )

        // And sending the action anyway — a greyed menu item cannot be clicked, but
        // the selector can be performed — asks the client for nothing, rather than
        // leaving it to refuse a mode the gateway already withheld.
        target.perform(ViewerMenus.autoResizeAction, with: nil)
        #expect(!sink.sent(.setAutoResize(true)))
    }

    /// And it is dead where a resize is not allowed at all, with the other two.
    @Test
    func theAutoResizeItemIsDeadWithoutThePermission() {
        let model = AppModel.underTest(sink: RecordingSink())
        model.apply(.state(AppModel.desktopState(canResize: false)))
        let target = ResizeMenuTarget(model: model)

        #expect(!target.validateMenuItem(item(ViewerMenus.autoResizeAction)))
    }

    /// "Resize to Display" is entirely local — the window takes the remote's size
    /// and nothing goes on the wire — so the hook the model calls is the whole of
    /// what forwarding means here.
    @Test
    func resizingToTheDisplayReachesTheModel() throws {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)
        var fitted = 0
        model.fitWindowToRemote = { fitted += 1 }
        let target = ResizeMenuTarget(model: model)

        model.apply(
            .state(
                AppModel.desktopState(
                    size: NativeState.RemoteSize(w: 3200, h: 2000, scale: 2)
                )
            )
        )
        #expect(model.canResizeToDisplay)

        // Sent the way the menu sends it, rather than by calling the method: this
        // is the half that a mistyped `@objc` name would break. Required first, so
        // a rename fails here rather than aborting the process — `perform` on a
        // selector an object does not answer raises, and an uncaught ObjC
        // exception takes the whole suite down with it.
        try #require(target.responds(to: ViewerMenus.resizeToDisplayAction))
        target.perform(ViewerMenus.resizeToDisplayAction, with: nil)

        #expect(fitted == 1)
        #expect(sink.commands.isEmpty, "and the remote is not told about it")
    }

    /// And the other direction, which does reach the client: one request, now.
    @Test
    func resizingToTheWindowReachesThePage() throws {
        let sink = RecordingSink()
        let model = AppModel.underTest(sink: sink)
        model.apply(.state(AppModel.desktopState(canResize: true)))
        let target = ResizeMenuTarget(model: model)

        try #require(target.responds(to: ViewerMenus.resizeToWindowAction))
        target.perform(ViewerMenus.resizeToWindowAction, with: nil)

        #expect(sink.commands == [.resizeToWindow])
    }

    /// Enablement comes from `validateMenuItem`, which AppKit calls as the menu
    /// opens. Each action has to read its *own* property: swapping them would grey
    /// the wrong item, and on RDP — where both are allowed — nothing would look
    /// wrong at all.
    @Test
    func eachItemIsValidatedAgainstItsOwnProperty() {
        // The row where the two answers differ: a target without `resize` cannot be
        // asked to match the window, but its desktop holds still and can be fitted
        // to.
        let model = AppModel.underTest(sink: RecordingSink())
        model.apply(.state(AppModel.desktopState(canResize: false)))
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
        let model = AppModel.underTest(sink: RecordingSink())
        model.apply(.state(AppModel.desktopState(canResize: true)))

        let target = ResizeMenuTarget(model: model)

        #expect(target.validateMenuItem(item(ViewerMenus.resizeToWindowAction)))
        #expect(target.validateMenuItem(item(ViewerMenus.resizeToDisplayAction)))
    }

    /// Off a session both are dead, which is the state the menu bar comes up in.
    @Test
    func neitherItemIsLiveWithoutADesktop() {
        let target = ResizeMenuTarget(model: AppModel.underTest(sink: RecordingSink()))

        #expect(!target.validateMenuItem(item(ViewerMenus.resizeToWindowAction)))
        #expect(!target.validateMenuItem(item(ViewerMenus.resizeToDisplayAction)))
    }

    /// This object is a menu item's target, and AppKit asks it about whatever item
    /// it is sent. Anything that is not one of ours is not this object's to grey —
    /// answering `false` by default would disable an item it knows nothing about.
    @Test
    func anItemThatIsNotOursIsLeftEnabled() {
        let target = ResizeMenuTarget(model: AppModel.underTest(sink: RecordingSink()))

        #expect(target.validateMenuItem(item(#selector(NSWindow.toggleFullScreen(_:)))))
        #expect(target.validateMenuItem(NSMenuItem()))
    }

    private func item(_ action: Selector) -> NSMenuItem {
        NSMenuItem(title: "", action: action, keyEquivalent: "")
    }
}
