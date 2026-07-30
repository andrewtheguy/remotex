import SwiftUI

/// Everything post-login: the remote surface, the target picker over it, and the
/// connection interstitial over both.
///
/// One screen rather than two because the picker and the desktop are the same
/// attached session — which of them is showing is the gateway's `picker` or
/// `connected`, and the framebuffer has to survive a trip to the picker and back.
struct DesktopScreen: View {
    let model: AppModel

    var body: some View {
        ZStack {
            RemoteSurfaceHost(
                model: model,
                remoteSize: model.session.remoteSize,
                guestScale: model.session.remoteScale,
                cursor: model.remoteCursor,
                // The toolbar is the desktop's to take while one is showing. In full
                // screen that is what lets the title bar auto-hide, so the remote
                // reaches the top of the screen and its own menu bar is a target
                // rather than a strip the window drags by.
                hidesToolbar: model.session.screen == .desktop
            )

            // The picker owns the screen only once the socket is really attached;
            // before that the interstitial is what should be visible.
            if model.session.screen == .picker,
               model.session.connectionStatus == .connected
            {
                TargetPickerView(model: model)
            }

            if model.showsStatusOverlay {
                StatusOverlayView(model: model)
            }
        }
    }
}
