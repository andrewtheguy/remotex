import AppKit
import Testing

@testable import RemotexViewer

/// Chromium will not run under an application object that cannot answer
/// `CrAppProtocol`, and it asks for it by selector rather than through any type.
/// Both names are `@objc` spellings, so a Swift rename on either one compiles,
/// links, ships, and then kills the app at the first event it handles with
/// `-[NSApplication isHandlingSendEvent]: unrecognized selector`.
struct ViewerApplicationTests {
    @Test func theApplicationAnswersWhatChromiumAsksIt() {
        // On the class rather than on `.shared`: naming the selectors is the whole
        // check, and instantiating the application object out of a test process
        // would make this depend on what any other test had already done to `NSApp`.
        #expect(ViewerApplication.instancesRespond(to: Selector(("isHandlingSendEvent"))))
        #expect(ViewerApplication.instancesRespond(to: Selector(("setHandlingSendEvent:"))))
    }
}
