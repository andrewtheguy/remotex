import AppKit

/// The target of the three resize items in AppKit's View menu.
///
/// They cannot be declared in `RemoteCommands` — see the comment there — so they
/// are `NSMenuItem`s, and an `NSMenuItem` needs an Objective-C object to send its
/// action to. This is that object, and nothing more: it forwards to the model and
/// answers whether each item should be live.
///
/// Enablement comes from `validateMenuItem` rather than being pushed onto the
/// items when the model changes. AppKit calls it as the menu opens, which is
/// exactly when the answer has to be right, and it means nothing has to observe
/// `canResizeNow`/`canResizeToDisplay` — both of which move with the window size,
/// the shared display and the target's own `resize`, and would otherwise need
/// three subscriptions to keep one menu item honest.
@MainActor
final class ResizeMenuTarget: NSObject, NSMenuItemValidation {
    private let model: AppModel

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    @objc(toggleAutoResizeFromMenu:)
    func toggleAutoResizeFromMenu(_ sender: Any?) {
        model.setAutoResize(!model.autoResizes)
    }

    @objc(resizeToWindowFromMenu:)
    func resizeToWindowFromMenu(_ sender: Any?) {
        model.resizeToWindow()
    }

    @objc(resizeToDisplayFromMenu:)
    func resizeToDisplayFromMenu(_ sender: Any?) {
        model.resizeToDisplay()
    }

    /// Auto Resize decides *how* this session resizes, and the other two are the
    /// one-shots for when it is off. They are not alternatives to each other: the
    /// first pushes this window's size to a remote that takes one, the second pulls
    /// the remote's size into this window and sends nothing, and which end to move
    /// is the user's call. Both grey out while auto is on — one is what auto does
    /// continuously, and the other cannot fit a window to a desktop that is already
    /// fitting itself to the window.
    ///
    /// A greyed item stays in the menu on purpose — what a session allows is worth
    /// reading off the three rather than inferring from an item that is not there.
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case ViewerMenus.autoResizeAction:
            // The tick and the title are set here rather than pushed on when the
            // model changes, for the same reason enablement is: AppKit asks as the
            // menu opens, which is exactly when the answer has to be right. The
            // title carries the one distinction greying cannot: this remote takes a
            // resize, just not one per window drag.
            menuItem.state = model.autoResizes ? .on : .off
            menuItem.title = model.autoResizeLabel
            return model.canAutoResize
        case ViewerMenus.resizeToWindowAction:
            return model.canResizeNow
        case ViewerMenus.resizeToDisplayAction:
            return model.canResizeToDisplay
        default:
            return true
        }
    }
}
