import AppKit
import Testing
@testable import RemotexViewer

struct KeyboardCaptureTests {
    /// The gate that decides whether a key event belongs to the remote. It has to
    /// answer yes for the surface and anything inside it, and no for the
    /// clipboard card's editor — otherwise typing clipboard text would also be
    /// typed on the remote.
    @Test
    @MainActor
    func theSurfaceAndItsSubviewsCaptureButANativeClipboardEditorDoesNot() {
        let surface = NSView(frame: .zero)
        let child = FocusableView(frame: .zero)
        surface.addSubview(child)
        let clipboardEditor = NSTextView(frame: .zero)

        #expect(KeyboardCapture.capturesFirstResponder(surface, inside: surface))
        #expect(KeyboardCapture.capturesFirstResponder(child, inside: surface))
        #expect(!KeyboardCapture.capturesFirstResponder(clipboardEditor, inside: surface))
        #expect(!KeyboardCapture.capturesFirstResponder(nil, inside: surface))
    }

    /// A sibling is not a descendant. Worth pinning because the clipboard card
    /// lives alongside the surface in the same window, not inside it.
    @Test
    @MainActor
    func aSiblingViewDoesNotCapture() {
        let window = NSView(frame: .zero)
        let surface = NSView(frame: .zero)
        let sibling = FocusableView(frame: .zero)
        window.addSubview(surface)
        window.addSubview(sibling)

        #expect(!KeyboardCapture.capturesFirstResponder(sibling, inside: surface))
    }
}

private final class FocusableView: NSView {
    override var acceptsFirstResponder: Bool { true }
}
