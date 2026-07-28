import AppKit

/// The target of the two resize items in AppKit's View menu.
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

    @objc(resizeToWindowFromMenu:)
    func resizeToWindowFromMenu(_ sender: Any?) {
        model.resizeToWindow()
    }

    @objc(resizeToDisplayFromMenu:)
    func resizeToDisplayFromMenu(_ sender: Any?) {
        model.resizeToDisplay()
    }

    /// The two directions a size mismatch can be settled, and they are not
    /// alternatives: the first pushes this window's size to a remote that takes
    /// one (RDP with `resize`, rxa on a display the agent made), the second pulls
    /// the remote's size into this window and sends nothing. A target that allows
    /// the first allows both, and which end to move is the user's call. VNC is the
    /// exception and greys the second, because a desktop that already follows the
    /// window cannot be fitted to it.
    ///
    /// A greyed item stays in the menu on purpose — which way a target allows is
    /// worth reading off the pair rather than inferring from an item that is not
    /// there.
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case ViewerMenus.resizeToWindowAction:
            return model.canResizeNow
        case ViewerMenus.resizeToDisplayAction:
            return model.canResizeToDisplay
        default:
            return true
        }
    }
}
